mod command;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};
use tb_client::{Client, ClientError};

use command::{Cli, Command};

/// Exit codes: 1 operation failed, 2 usage error (clap), 3 daemon unreachable.
const EXIT_FAILED: u8 = 1;
const EXIT_UNREACHABLE: u8 = 3;

fn print_json(value: &Value, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("JSON value serializes");
    println!("{text}");
}

fn fail(code: &str, message: &str, exit: u8) -> ExitCode {
    eprintln!("{}", json!({"error": {"code": code, "message": message}}));
    ExitCode::from(exit)
}

fn ops_listing() -> Value {
    let list: Vec<Value> = tb_protocol::ops()
        .iter()
        .map(|o| {
            json!({
                "name": o.name,
                "module": o.module,
                "access": format!("{:?}", o.access).to_lowercase(),
                "description": o.description,
            })
        })
        .collect();
    Value::Array(list)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let pretty = cli.pretty || std::io::stdout().is_terminal();

    if let Command::Mcp(args) = &cli.command {
        let daemon = tb_client::Daemon::new(cli.connection_file.clone(), "tb-mcp");
        let result = match args.http {
            Some(addr) => tb_mcp::serve_http(daemon, addr, args.allow_hosts.clone()).await,
            None => tb_mcp::serve_stdio(daemon).await,
        };
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail("mcp_failed", &format!("{err:#}"), EXIT_FAILED),
        };
    }
    if matches!(cli.command, Command::Ops) {
        print_json(&ops_listing(), pretty);
        return ExitCode::SUCCESS;
    }

    let (op, params) = match cli.command.to_request() {
        Ok(Some(request)) => request,
        Ok(None) => unreachable!("ops and mcp are handled above"),
        Err(err) => return fail("bad_request", &format!("{err:#}"), 2),
    };

    let path = cli.connection_file.clone().unwrap_or_else(tb_protocol::connection::default_path);
    let client = match Client::connect_file(&path, "tb").await {
        Ok(client) => client,
        Err(err) => return fail("daemon_unavailable", &err.to_string(), EXIT_UNREACHABLE),
    };
    let timeout = cli.timeout.map(Duration::from_secs);
    match client.request(&op, params, timeout).await {
        Ok(result) => {
            print_json(&result, pretty);
            ExitCode::SUCCESS
        }
        Err(err) => {
            let body = err.to_body();
            let exit = if matches!(err, ClientError::Connect(_)) { EXIT_UNREACHABLE } else { EXIT_FAILED };
            fail(&body.code, &body.message, exit)
        }
    }
}
