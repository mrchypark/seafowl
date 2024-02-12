use crate::statements::create_table_and_insert;
use arrow::record_batch::RecordBatch;
use arrow_flight::error::Result;
use arrow_flight::sql::{CommandStatementQuery, ProstMessageExt};
use arrow_flight::{FlightClient, FlightDescriptor};
use datafusion_common::assert_batches_eq;
use futures::TryStreamExt;
use prost::Message;
use seafowl::config::context::build_context;
use seafowl::config::schema::{load_config_from_string, SeafowlConfig};
use seafowl::context::SeafowlContext;
use seafowl::frontend::flight::run_flight_server;
use std::sync::Arc;
use tokio::net::TcpListener;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

mod client;
mod search_path;

async fn make_test_context() -> (SeafowlConfig, Arc<SeafowlContext>) {
    // let OS choose a free port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config_text = format!(
        r#"
[object_store]
type = "memory"

[catalog]
type = "sqlite"
dsn = ":memory:"

[frontend.flight]
bind_host = "127.0.0.1"
bind_port = {}"#,
        addr.port()
    );

    let config = load_config_from_string(&config_text, false, None).unwrap();
    let context = Arc::from(build_context(&config).await.unwrap());
    (config, context)
}

async fn flight_server() -> (Arc<SeafowlContext>, FlightClient) {
    let (config, context) = make_test_context().await;

    let flight_cfg = config
        .frontend
        .flight
        .expect("Arrow Flight frontend configured");

    let flight = run_flight_server(context.clone(), flight_cfg.clone());
    tokio::task::spawn(flight);

    // Create the channel for the client
    let channel = Channel::from_shared(format!(
        "http://{}:{}",
        flight_cfg.bind_host, flight_cfg.bind_port
    ))
    .expect("Endpoint created")
    .connect_lazy();

    (context, FlightClient::new(channel))
}

async fn get_flight_batches(
    client: &mut FlightClient,
    query: String,
) -> Result<Vec<RecordBatch>> {
    let cmd = CommandStatementQuery {
        query,
        transaction_id: None,
    };
    let request = FlightDescriptor::new_cmd(cmd.as_any().encode_to_vec());
    let response = client.get_flight_info(request).await?;

    // Get the returned ticket
    let ticket = response.endpoint[0]
        .ticket
        .clone()
        .expect("expected ticket");

    // Retrieve the corresponding Flight stream and collect into batches
    let flight_stream = client.do_get(ticket).await?;

    let batches = flight_stream.try_collect().await?;

    Ok(batches)
}
