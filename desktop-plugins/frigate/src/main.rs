mod config;
#[path = "../../camera-common/src/exclusions.rs"]
mod exclusions;
mod frigate;
mod media;
mod network;
mod wire;

use serde_json::json;
use std::{process::ExitCode, time::Duration};
use tokio::sync::mpsc;
use wire::{HostFrame, Output, StopReason, PLUGIN_ID};

async fn next_frame(incoming: &mut mpsc::Receiver<HostFrame>) -> Result<HostFrame, &'static str> {
    tokio::time::timeout(Duration::from_secs(10), incoming.recv())
        .await
        .map_err(|_| "host handshake timed out")?
        .ok_or("host input closed")
}

async fn session(
    incoming: &mut mpsc::Receiver<HostFrame>,
    output: &Output,
) -> Result<(), &'static str> {
    let api = wire::validate_hello(next_frame(incoming).await?)?;
    output.send(json!({"type":"ready","protocol_version":1,"host_api_version":api,"plugin_id":PLUGIN_ID})).await?;
    let HostFrame::Configuration { configuration } = next_frame(incoming).await? else {
        return Err("configuration required");
    };
    configuration.validate()?;
    output
        .send(json!({"type":"configuration_ready","revision":configuration.revision}))
        .await?;
    // A configuration acknowledgement must be flushed before any network work.
    // Changes use a fresh process; there is no runtime reconfiguration channel.
    tokio::select! {
        biased;
        _=incoming.recv()=>Err("unexpected host command"),
        result=network::run(configuration,output,wire::supports_http_video(&api),wire::supports_http_live(&api))=>result,
    }
}

async fn run() -> Result<(), &'static str> {
    let (mut incoming, output, mut stopped) = wire::stdio();
    let result = tokio::select! {
        biased;
        reason=stopped.wait_for(|reason| *reason!=StopReason::Running)=>match reason {
            Ok(reason) if *reason==StopReason::Shutdown || *reason==StopReason::Eof => Ok(()),
            _=>Err("invalid host input"),
        },
        result=session(&mut incoming,&output)=>result,
    };
    // The reader signals stop before dropping its command sender. If that
    // closure wins an inner select, EOF/shutdown still has clean-exit semantics.
    if matches!(*stopped.borrow(), StopReason::Shutdown | StopReason::Eof) {
        Ok(())
    } else {
        result
    }
}

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|_| eprintln!("Frigate worker failed")));
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("Frigate worker could not start");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run());
    // DNS and platform trust loading may use a blocking pool; neither can delay
    // process exit after the host revokes the session or closes its pipes.
    runtime.shutdown_timeout(Duration::from_millis(100));
    if result.is_err() {
        eprintln!("Frigate worker session failed");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
