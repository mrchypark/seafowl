use arrow::csv::ReaderBuilder;
use arrow::datatypes::{DataType, Field, Schema};
use std::io::Cursor;
use std::{net::SocketAddr, sync::Arc};

use arrow::json::LineDelimitedWriter;
use arrow::record_batch::RecordBatch;
use bytes::BufMut;
use bytes::Bytes;
use datafusion::parquet::arrow::{ArrowReader, ParquetFileArrowReader};
use datafusion::physical_plan::memory::MemoryExec;
use datafusion::{
    datasource::DefaultTableSource,
    logical_plan::{LogicalPlan, PlanVisitor, TableScan},
};
use futures::TryStreamExt;
use hex::encode;
use log::debug;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use warp::multipart::{FormData, Part};
use warp::reply::Response;
use warp::{hyper::StatusCode, Filter, Reply};

use crate::{
    config::schema::HttpFrontend, context::SeafowlContext, data_types::TableVersionId,
    provider::SeafowlTable,
};

const QUERY_HEADER: &str = "X-Seafowl-Query";
const IF_NONE_MATCH: &str = "If-None-Match";
const ETAG: &str = "ETag";

#[derive(Default)]
struct ETagBuilderVisitor {
    table_versions: Vec<TableVersionId>,
}

impl PlanVisitor for ETagBuilderVisitor {
    type Error = ();

    fn pre_visit(&mut self, plan: &LogicalPlan) -> Result<bool, Self::Error> {
        if let LogicalPlan::TableScan(TableScan { source, .. }) = plan {
            // TODO handle external Parquet tables too
            if let Some(default_table_source) =
                source.as_any().downcast_ref::<DefaultTableSource>()
            {
                if let Some(table) = default_table_source
                    .table_provider
                    .as_any()
                    .downcast_ref::<SeafowlTable>()
                {
                    self.table_versions.push(table.table_version_id)
                }
            }
        }
        Ok(true)
    }
}

fn plan_to_etag(plan: &LogicalPlan) -> String {
    let mut visitor = ETagBuilderVisitor::default();
    plan.accept(&mut visitor).unwrap();

    debug!("Extracted table versions: {:?}", visitor.table_versions);

    let mut hasher = Sha256::new();
    hasher.update(json!(visitor.table_versions).to_string());
    encode(hasher.finalize())
}

#[derive(Debug, Deserialize)]
struct QueryBody {
    query: String,
}

/// POST /q
pub async fn uncached_read_write_query(
    query: String,
    context: Arc<dyn SeafowlContext>,
) -> Response {
    context.reload_schema().await;
    // TODO: handle/propagate errors
    // TODO (when authz is implemented) check for read-only queries
    let physical = context.plan_query(&query).await.unwrap();
    let batches = context.collect(physical).await.unwrap();

    let mut buf = Vec::new();
    let mut writer = LineDelimitedWriter::new(&mut buf);
    writer.write_batches(&batches).unwrap();
    writer.finish().unwrap();

    buf.into_response()
}

/// GET /q/[query hash]
pub async fn cached_read_query(
    query_hash: String,
    query: String,
    if_none_match: Option<String>,
    context: Arc<dyn SeafowlContext>,
) -> Response {
    // Ignore dots at the end
    let query_hash = query_hash.split('.').next().unwrap();

    context.reload_schema().await;
    let mut hasher = Sha256::new();
    hasher.update(&query);
    let hash_str = encode(hasher.finalize());

    debug!(
        "Received query: {}, URL hash {}, actual hash {}",
        query, query_hash, hash_str
    );

    // Verify the query hash matches the query
    if query_hash != hash_str {
        return warp::reply::with_status("HASH_MISMATCH", StatusCode::BAD_REQUEST)
            .into_response();
    }

    // Plan the query
    // TODO handle error
    let plan = context.create_logical_plan(&query).await.unwrap();
    debug!("Query plan: {:?}", plan);

    // Write queries should come in as POST requests
    match plan {
        LogicalPlan::CreateExternalTable(_)
        | LogicalPlan::CreateMemoryTable(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::CreateCatalogSchema(_)
        | LogicalPlan::CreateCatalog(_)
        | LogicalPlan::DropTable(_)
        | LogicalPlan::Analyze(_)
        | LogicalPlan::Extension(_) => {
            return warp::reply::with_status(
                "NOT_READ_ONLY_QUERY",
                StatusCode::METHOD_NOT_ALLOWED,
            )
            .into_response()
        }
        _ => (),
    };

    // Pre-execution check: if ETags match, we don't need to re-execute the query
    let etag = plan_to_etag(&plan);
    debug!("ETag: {}, if-none-match header: {:?}", etag, if_none_match);

    if let Some(if_none_match) = if_none_match {
        if etag == if_none_match {
            return warp::reply::with_status("NOT_MODIFIED", StatusCode::NOT_MODIFIED)
                .into_response();
        }
    }

    // Guess we'll have to actually run the query
    let physical = context.create_physical_plan(&plan).await.unwrap();
    let batches = context.collect(physical).await.unwrap();

    let mut buf = Vec::new();
    let mut writer = LineDelimitedWriter::new(&mut buf);
    writer.write_batches(&batches).unwrap();
    writer.finish().unwrap();

    warp::reply::with_header(buf, ETAG, etag).into_response()
}

/// POST /upload/[schema]/[table]
pub async fn upload(
    _schema_name: String,
    table_name: String,
    form: FormData,
    context: Arc<dyn SeafowlContext>,
) -> Response {
    let parts: Vec<Part> = form.try_collect().await.unwrap();
    for p in parts {
        if p.name() == "file" {
            let filename = p.filename().unwrap().to_string();

            // Load the file content from the request
            let value = p
                .stream()
                .try_fold(Vec::new(), |mut vec, data| {
                    vec.put(data);
                    async move { Ok(vec) }
                })
                .await
                .map_err(|e| {
                    eprintln!("Error reading part's data: {}", e);
                    warp::reject::reject()
                })
                .unwrap();
            let mut cursor = Cursor::new(&value);

            let schema: Schema;
            let partition = if filename.ends_with(".csv") {
                schema = Schema::new(vec![
                    Field::new("fruit_id", DataType::Int8, false),
                    Field::new("name", DataType::Utf8, false),
                ]);

                let builder = ReaderBuilder::new()
                    .with_schema(Arc::new(schema.clone()))
                    .has_header(true)
                    .with_escape(b'\\'); // default is None, change to \

                let csv_reader = builder.build(&mut cursor).unwrap();
                let partition: Vec<RecordBatch> =
                    csv_reader.into_iter().map(|item| item.unwrap()).collect();

                partition
            } else if filename.ends_with(".parquet") {
                let mut parquet_reader =
                    ParquetFileArrowReader::try_new(Bytes::from(value)).unwrap();

                schema = parquet_reader.get_schema().unwrap();

                let partition: Vec<RecordBatch> = parquet_reader
                    .get_record_reader(100000)
                    .unwrap()
                    .map(|item| item.unwrap())
                    .collect();

                partition
            } else {
                return warp::reply::with_status(
                    format!("File {} not supported", filename),
                    StatusCode::BAD_REQUEST,
                )
                .into_response();
            };

            let execution_plan = Arc::new(
                MemoryExec::try_new(&[partition], Arc::new(schema.clone()), None)
                    .unwrap(),
            );

            let _result = context
                .plan_to_table(execution_plan, table_name.clone())
                .await;
        }
    }
    warp::reply::with_status(Ok("done"), StatusCode::OK).into_response()
}

pub fn filters(
    context: Arc<dyn SeafowlContext>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["X-Seafowl-Query", "Authorization", "Content-Type"])
        .allow_methods(vec!["GET", "POST"]);

    // Cached read query
    let ctx = context.clone();
    let cached_read_query_route = warp::path!("q" / String)
        .and(warp::get())
        .and(
            // Extract the query either from the header or from the JSON body
            warp::header::<String>(QUERY_HEADER)
                .or(warp::body::json().map(|b: QueryBody| b.query))
                .unify(),
        )
        .and(warp::header::optional::<String>(IF_NONE_MATCH))
        .and(warp::any().map(move || ctx.clone()))
        .then(cached_read_query);

    // Uncached read/write query
    let ctx = context.clone();
    let uncached_read_write_query_route = warp::path!("q")
        .and(warp::post())
        .and(
            // Extract the query from the JSON body
            warp::body::json().map(|b: QueryBody| b.query),
        )
        .and(warp::any().map(move || ctx.clone()))
        .then(uncached_read_write_query);

    // Upload endpoint
    let ctx = context.clone();
    let upload_route = warp::path!("upload" / String / String)
        .and(warp::post())
        .and(warp::multipart::form())
        .and(warp::any().map(move || ctx.clone()))
        .then(upload);

    cached_read_query_route
        .or(uncached_read_write_query_route)
        .with(cors)
        .or(upload_route)
}

pub async fn run_server(context: Arc<dyn SeafowlContext>, config: HttpFrontend) {
    let filters = filters(context);

    let socket_addr: SocketAddr = format!("{}:{}", config.bind_host, config.bind_port)
        .parse()
        .expect("Error parsing the listen address");
    warp::serve(filters).run(socket_addr).await;
}

#[cfg(test)]
mod tests {
    use arrow::array::{Int32Array, StringArray};
    use arrow::csv::Writer;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::assert_batches_eq;
    use datafusion::from_slice::FromSlice;
    use datafusion::parquet::arrow::ArrowWriter;
    use std::io::Cursor;
    use std::{collections::HashMap, sync::Arc};

    use warp::{
        hyper::{header::IF_NONE_MATCH, StatusCode},
        test::request,
    };

    use test_case::test_case;

    use crate::{
        context::{test_utils::in_memory_context, SeafowlContext},
        frontend::http::{filters, ETAG, QUERY_HEADER},
    };

    /// Build an in-memory context with a single table
    /// We implicitly assume here that this table is the only one in this context
    /// and has version ID 1 (otherwise the hashes won't match).
    async fn in_memory_context_with_single_table() -> Arc<dyn SeafowlContext> {
        let context = Arc::new(in_memory_context().await);

        context
            .collect(
                context
                    .plan_query("CREATE TABLE test_table(col_1 INTEGER)")
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        context.reload_schema().await;

        context
            .collect(
                context
                    .plan_query("INSERT INTO test_table VALUES (1)")
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        context.reload_schema().await;
        context
    }

    async fn in_memory_context_with_modified_table() -> Arc<dyn SeafowlContext> {
        let context = in_memory_context_with_single_table().await;
        context
            .collect(
                context
                    .plan_query("INSERT INTO test_table VALUES (2)")
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        context.reload_schema().await;
        context
    }

    const SELECT_QUERY: &str = "SELECT COUNT(*) AS c FROM test_table";
    const INSERT_QUERY: &str = "INSERT INTO test_table VALUES (2)";
    const CREATE_QUERY: &str = "CREATE TABLE other_test_table(col_1 INTEGER)";
    const SELECT_QUERY_HASH: &str =
        "7fbbf7dddfd330d03e5e08cc5885ad8ca823e1b56e7cbadd156daa0e21c288f6";
    const CREATE_QUERY_HASH: &str =
        "be185830b7db691f3ffd33c81a83bb4ed48e2411fc3fc500ee20b8ec7effb8a6";
    const V1_ETAG: &str =
        "038966de9f6b9a901b20b4c6ca8b2a46009feebe031babc842d43690c0bc222b";
    const V2_ETAG: &str =
        "06d033ece6645de592db973644cf7357255f24536ff7b03c3b2ace10736f7636";

    #[tokio::test]
    async fn test_get_cached_hash_mismatch() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path("/q/wrong-hash")
            .header(QUERY_HEADER, SELECT_QUERY)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.body(), "HASH_MISMATCH");
    }

    #[tokio::test]
    async fn test_get_cached_write_query_error() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", CREATE_QUERY_HASH).as_str())
            .header(QUERY_HEADER, CREATE_QUERY)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.body(), "NOT_READ_ONLY_QUERY");
    }

    #[tokio::test]
    async fn test_get_cached_no_etag() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", SELECT_QUERY_HASH).as_str())
            .header(QUERY_HEADER, SELECT_QUERY)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":1}\n");
        assert_eq!(resp.headers().get(ETAG).unwrap().to_str().unwrap(), V1_ETAG);
    }

    #[tokio::test]
    async fn test_get_cached_no_query() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", SELECT_QUERY_HASH).as_str())
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // TODO: return a better error than this
        // https://github.com/splitgraph/seafowl/issues/22
        assert_eq!(resp.body(), "Request body deserialize error: EOF while parsing a value at line 1 column 0");
    }

    #[tokio::test]
    async fn test_get_cached_no_etag_query_in_body() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", SELECT_QUERY_HASH).as_str())
            .json(&HashMap::from([("query", SELECT_QUERY)]))
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":1}\n");
        assert_eq!(resp.headers().get(ETAG).unwrap().to_str().unwrap(), V1_ETAG);
    }

    #[tokio::test]
    async fn test_get_cached_no_etag_extension() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}.bin", SELECT_QUERY_HASH).as_str())
            .header(QUERY_HEADER, SELECT_QUERY)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":1}\n");
        assert_eq!(resp.headers().get(ETAG).unwrap().to_str().unwrap(), V1_ETAG);
    }

    #[tokio::test]
    async fn test_get_cached_reuse_etag() {
        // Pass the same ETag as If-None-Match, should return a 301

        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", SELECT_QUERY_HASH).as_str())
            .header(QUERY_HEADER, SELECT_QUERY)
            .header(IF_NONE_MATCH, V1_ETAG)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.body(), "NOT_MODIFIED");
    }

    #[tokio::test]
    async fn test_get_cached_etag_new_version() {
        // Pass the same ETag as If-None-Match, but the table version changed -> reruns the query

        let context = in_memory_context_with_modified_table().await;
        let handler = filters(context);

        let resp = request()
            .method("GET")
            .path(format!("/q/{}", SELECT_QUERY_HASH).as_str())
            .header(QUERY_HEADER, SELECT_QUERY)
            .header(ETAG, V1_ETAG)
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":2}\n");
        assert_eq!(resp.headers().get(ETAG).unwrap().to_str().unwrap(), V2_ETAG);
    }

    #[tokio::test]
    async fn test_get_uncached_read_query() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("POST")
            .path("/q")
            .json(&HashMap::from([("query", SELECT_QUERY)]))
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":1}\n");
    }

    #[tokio::test]
    async fn test_get_uncached_write_query() {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context);

        let resp = request()
            .method("POST")
            .path("/q")
            .json(&HashMap::from([("query", INSERT_QUERY)]))
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "");

        let resp = request()
            .method("POST")
            .path("/q")
            .json(&HashMap::from([("query", SELECT_QUERY)]))
            .reply(&handler)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "{\"c\":2}\n");
    }

    #[test_case(
        "csv";
        "CSV file upload")
    ]
    #[test_case(
        "parquet";
        "Parquet file upload")
    ]
    #[tokio::test]
    async fn test_upload(file_format: &str) {
        let context = in_memory_context_with_single_table().await;
        let handler = filters(context.clone());

        let table_name = format!("{}_table", file_format);

        // Prepare the schema + data (record batch) which we'll convert to bytes via corresponding writer
        let schema = Arc::new(Schema::new(vec![
            Field::new("fruit_id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, false),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from_slice(vec![1, 2])),
                Arc::new(StringArray::from(vec!["apple", "orange"])),
            ],
        )
        .unwrap();

        // Write out the CSV/Parquet format data into mock request
        let mut form_data: Cursor<Vec<u8>> = Default::default();
        // drop the writer early to release the borrow.
        if file_format == "csv" {
            let mut writer = Writer::new(&mut form_data);
            writer.write(&batch).unwrap();
        } else if file_format == "parquet" {
            let mut writer = ArrowWriter::try_new(&mut form_data, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }
        form_data.set_position(0);

        let mut body = format!(
            "--42\r\n\
            Content-Disposition: form-data; name=\"file\"; filename=\"fruits.{}\"\n\
            Content-Type: application/octet-stream\n\n",
            file_format
        )
        .as_bytes()
        .to_vec();

        let mut part_footer = "--42--".as_bytes().to_vec();

        body.append(&mut form_data.into_inner());
        body.append(&mut part_footer);

        let resp = request()
            .method("POST")
            .path(format!("/upload/{}/{}", "test_upload", table_name).as_str())
            .header("Host", "localhost:3030")
            .header("User-Agent", "curl/7.64.1")
            .header("Accept", "*/*")
            .header("Content-Length", 232)
            .header("Content-Type", "multipart/form-data; boundary=42")
            .body(body)
            .reply(&handler)
            .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), "done");

        context.reload_schema().await;

        let plan = context
            .plan_query(format!("SELECT * FROM {}", table_name).as_str())
            .await
            .unwrap();
        let results = context.collect(plan).await.unwrap();

        let expected = vec![
            "+----------+--------+",
            "| fruit_id | name   |",
            "+----------+--------+",
            "| 1        | apple  |",
            "| 2        | orange |",
            "+----------+--------+",
        ];
        assert_batches_eq!(expected, &results);
    }
}
