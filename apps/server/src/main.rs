use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
use notryx_server::config::Config;
use notryx_server::http::RunningServer;
use notryx_server::runtime::{WorkspaceRuntime, parse_admin_id};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;

const HELP: &str = "사용법:
  notryx-server init|serve|migrate --config <설정 파일>
  notryx-server adopt --config <설정 파일> <루트 파일 이름>
  notryx-server admin --config <설정 파일> status
  notryx-server admin --config <설정 파일> create-principal <reader|editor>
  notryx-server admin --config <설정 파일> set-role <principal UUID> <reader|editor>
  notryx-server admin --config <설정 파일> transfer-owner <principal UUID> <이전 owner의 reader|editor 역할>
  notryx-server admin --config <설정 파일> issue-pairing <principal UUID>
  notryx-server admin --config <설정 파일> revoke-device <device UUID>

init, migrate와 adopt는 서버를 중지한 상태에서 실행합니다.
issue-pairing의 JSON 표준 출력에는 일회용 credential이 포함됩니다.";

enum Command {
    Initialize,
    Serve,
    Migrate,
    Adopt(String),
    Admin(AdminRequest),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("서버 작업을 완료하지 못했습니다: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn role(value: &OsStr) -> Result<WorkspaceRole, &'static str> {
    match value.to_str() {
        Some("reader") => Ok(WorkspaceRole::Reader),
        Some("editor") => Ok(WorkspaceRole::Editor),
        _ => Err("invalid_request"),
    }
}

fn identifier(value: &OsStr) -> Result<String, &'static str> {
    parse_admin_id(value.to_str().ok_or("invalid_request")?)
        .map(|id| id.to_string())
        .map_err(|_| "invalid_request")
}

fn parse(arguments: &[OsString]) -> Result<(PathBuf, Command), &'static str> {
    let [command, flag, path, rest @ ..] = arguments else {
        return Err("invalid_request; 명령 안내는 --help를 사용하세요");
    };
    if flag != "--config" {
        return Err("invalid_request");
    }
    let command = match (command.to_str(), rest) {
        (Some("init"), []) => Command::Initialize,
        (Some("serve"), []) => Command::Serve,
        (Some("migrate"), []) => Command::Migrate,
        (Some("adopt"), [name]) => {
            let name = name.to_str().ok_or("invalid_request")?;
            notryx_domain::mutation::validate_name(name).map_err(|_| "invalid_request")?;
            Command::Adopt(name.to_owned())
        }
        (Some("admin"), [action, values @ ..]) => {
            let request = match (action.to_str(), values) {
                (Some("status"), []) => AdminRequest::Status {},
                (Some("create-principal"), [value]) => {
                    AdminRequest::CreatePrincipal { role: role(value)? }
                }
                (Some("set-role"), [principal, value]) => AdminRequest::SetRole {
                    principal_id: identifier(principal)?,
                    role: role(value)?,
                },
                (Some("transfer-owner"), [principal, value]) => AdminRequest::TransferOwner {
                    principal_id: identifier(principal)?,
                    previous_role: role(value)?,
                },
                (Some("issue-pairing"), [principal]) => AdminRequest::IssuePairing {
                    principal_id: identifier(principal)?,
                },
                (Some("revoke-device"), [device]) => AdminRequest::RevokeDevice {
                    device_id: identifier(device)?,
                },
                _ => return Err("invalid_request"),
            };
            Command::Admin(request)
        }
        _ => return Err("invalid_request"),
    };
    Ok((PathBuf::from(path), command))
}

fn print_json(value: &impl serde::Serialize) -> Result<(), &'static str> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|_| "output_failed")?;
    stdout.write_all(b"\n").map_err(|_| "output_failed")?;
    stdout.flush().map_err(|_| "output_failed")
}

fn admin_error(code: &str) -> &'static str {
    match code {
        "unauthorized" => "unauthorized",
        "invalid_request" => "invalid_request",
        "forbidden" => "forbidden",
        "not_found" => "not_found",
        "storage_error" => "storage_error",
        "busy" => "busy",
        "closed" => "closed",
        "recovery_required" => "recovery_required",
        "runtime_error" => "runtime_error",
        "migration_required" => "migration_required",
        "locked" => "locked",
        _ => "invalid_admin_response",
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.as_slice() == [OsStr::new("--help")] {
        println!("{HELP}");
        return Ok(());
    }
    let (path, command) = parse(&arguments)?;
    let config = Config::load(path)?;
    match command {
        Command::Initialize => {
            WorkspaceRuntime::initialize(config.workspace).await?;
            println!("workspace_initialized");
        }
        Command::Migrate => {
            WorkspaceRuntime::migrate(config.workspace).await?;
            println!("workspace_migrated");
        }
        Command::Adopt(name) => {
            let node_id = WorkspaceRuntime::adopt(config.workspace, name).await?;
            print_json(&serde_json::json!({ "node_id": node_id }))?;
        }
        Command::Admin(request) => {
            let allow_pairing_output = matches!(request, AdminRequest::IssuePairing { .. });
            match notryx_server::admin::request(&config.server.admin_socket, &request).await? {
                AdminResponse::Ok { result } => {
                    if matches!(result, AdminResult::Pairing { .. }) && !allow_pairing_output {
                        return Err("invalid_admin_response".into());
                    }
                    print_json(&result)?;
                }
                AdminResponse::Error { code } => return Err(admin_error(&code).into()),
            }
        }
        Command::Serve => serve(config).await?,
    }
    Ok(())
}

async fn serve(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal_setup_failed")?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|_| "signal_setup_failed")?;
    let server = RunningServer::start(config).await?;
    println!("server_started");
    std::io::stdout()
        .flush()
        .map_err(|_| "startup_output_failed")?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    server.shutdown().await?;
    println!("server_stopped");
    Ok(())
}
