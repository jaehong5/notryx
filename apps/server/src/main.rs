use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, WorkspaceRole};
use notryx_server::config::Config;
use notryx_server::http::RunningServer;
use notryx_server::runtime::{WorkspaceRuntime, parse_admin_id};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;

use notryx_server::cold_backup;
mod healthcheck;

const HELP: &str = "사용법:
  notryx-server init|serve|migrate --config <설정 파일>
  notryx-server backup --config <설정 파일> <새 백업 경로>
  notryx-server backup-online --config <설정 파일> <새 백업 경로>
  notryx-server backup-verify <백업 경로>
  notryx-server restore <백업 경로> <새 복원 경로>
  notryx-server healthcheck --address <IP:port>
  notryx-server adopt --config <설정 파일> <루트 파일 이름>
  notryx-server admin --config <설정 파일> status
  notryx-server admin --config <설정 파일> journal-status
  notryx-server admin --config <설정 파일> unsupported-items [snapshot UUID와 offset]
  notryx-server admin --config <설정 파일> create-principal <reader|editor>
  notryx-server admin --config <설정 파일> set-role <principal UUID> <reader|editor>
  notryx-server admin --config <설정 파일> transfer-owner <principal UUID> <이전 owner의 reader|editor 역할>
  notryx-server admin --config <설정 파일> issue-pairing <principal UUID>
  notryx-server admin --config <설정 파일> revoke-device <device UUID>
  notryx-server admin --config <설정 파일> recovery-list [after operation UUID]
  notryx-server admin --config <설정 파일> recovery-inspect <operation UUID> [after SHA-256 hex]
  notryx-server admin --config <설정 파일> recovery-resolve <operation UUID>
  notryx-server admin --config <설정 파일> recovery-release <operation UUID>

init, migrate와 adopt는 서버를 중지한 상태에서 실행합니다.
recovery-resolve는 외부 파일 변경을 중지하고 자료를 대조한 뒤 현재 파일 상태를 채택합니다.
복구 응답 대기는 최대 120초이며 응답을 잃으면 같은 operation UUID로 다시 요청합니다.
recovery-release는 해결한 작업의 격리 사본 보호를 해제합니다.
issue-pairing의 JSON 표준 출력에는 일회용 credential이 포함됩니다.";

enum Command {
    Initialize,
    Serve,
    Migrate,
    Adopt(String),
    Backup(PathBuf),
    OnlineBackup(PathBuf),
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

fn operation_id(value: &OsStr) -> Result<notryx_domain::OperationId, &'static str> {
    value
        .to_str()
        .ok_or("invalid_request")?
        .parse()
        .map_err(|_| "invalid_request")
}

fn recovery_hash(value: &OsStr) -> Result<String, &'static str> {
    let hash = value.to_str().ok_or("invalid_request")?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("invalid_request");
    }
    Ok(hash.to_owned())
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
        (Some("backup"), [destination]) => Command::Backup(PathBuf::from(destination)),
        (Some("backup-online"), [destination]) => Command::OnlineBackup(PathBuf::from(destination)),
        (Some("admin"), [action, values @ ..]) => {
            let request = match (action.to_str(), values) {
                (Some("status"), []) => AdminRequest::Status {},
                (Some("journal-status"), []) => AdminRequest::JournalStatus {},
                (Some("unsupported-items"), []) => AdminRequest::UnsupportedItems {
                    snapshot_id: None,
                    offset: 0,
                    limit: 20,
                },
                (Some("unsupported-items"), [snapshot, offset]) => {
                    let snapshot = snapshot.to_str().ok_or("invalid_request")?;
                    let parsed = uuid::Uuid::parse_str(snapshot).map_err(|_| "invalid_request")?;
                    if parsed.get_version_num() != 7 || parsed.to_string() != snapshot {
                        return Err("invalid_request");
                    }
                    let offset = offset
                        .to_str()
                        .ok_or("invalid_request")?
                        .parse::<u16>()
                        .map_err(|_| "invalid_request")?;
                    AdminRequest::UnsupportedItems {
                        snapshot_id: Some(snapshot.to_owned()),
                        offset,
                        limit: 20,
                    }
                }
                (Some("recovery-list"), []) => AdminRequest::RecoveryList {
                    after_operation_id: None,
                    limit: 20,
                },
                (Some("recovery-list"), [after]) => AdminRequest::RecoveryList {
                    after_operation_id: Some(operation_id(after)?),
                    limit: 20,
                },
                (Some("recovery-inspect"), [operation]) => AdminRequest::RecoveryInspect {
                    operation_id: operation_id(operation)?,
                    after_hash: None,
                },
                (Some("recovery-inspect"), [operation, after]) => AdminRequest::RecoveryInspect {
                    operation_id: operation_id(operation)?,
                    after_hash: Some(recovery_hash(after)?),
                },
                (Some("recovery-resolve"), [operation]) => AdminRequest::RecoveryResolve {
                    operation_id: operation_id(operation)?,
                },
                (Some("recovery-release"), [operation]) => AdminRequest::RecoveryRelease {
                    operation_id: operation_id(operation)?,
                },
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
        "insufficient_storage" => "insufficient_storage",
        "busy" => "busy",
        "closed" => "closed",
        "recovery_required" => "recovery_required",
        "runtime_error" => "runtime_error",
        "migration_required" => "migration_required",
        "locked" => "locked",
        "external_change" => "external_change",
        "operation_pending" => "operation_pending",
        "backup_target_exists" => "backup_target_exists",
        "backup_timeout" => "backup_timeout",
        "backup_snapshot_timeout" => "backup_snapshot_timeout",
        "backup_invalid" => "backup_invalid",
        "backup_cancelled" => "backup_cancelled",
        "backup_recovery_required" => "backup_recovery_required",
        "backup_insufficient_space" => "backup_insufficient_space",
        "backup_publication_indeterminate" => "backup_publication_indeterminate",
        "backup_failed" => "backup_failed",
        "backup_manifest_too_large" => "backup_manifest_too_large",
        _ => "invalid_admin_response",
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|value| value == "healthcheck")
    {
        healthcheck::run(&arguments[1..])?;
        return Ok(());
    }
    if arguments.as_slice() == [OsStr::new("--help")] {
        println!("{HELP}");
        return Ok(());
    }
    if let [command, path] = arguments.as_slice()
        && command == "backup-verify"
    {
        cold_backup::verify(&PathBuf::from(path))?;
        println!("backup_verified");
        return Ok(());
    }
    if let [command, backup, destination] = arguments.as_slice()
        && command == "restore"
    {
        cold_backup::restore(&PathBuf::from(backup), &PathBuf::from(destination)).await?;
        println!("workspace_restored");
        return Ok(());
    }
    let (path, command) = parse(&arguments)?;
    let config = Config::load(&path)?;
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
        Command::Backup(destination) => {
            cold_backup::create(&config, &path, &destination).await?;
            println!("backup_created");
        }
        Command::OnlineBackup(destination) => {
            let path = std::path::absolute(path)?;
            let request = AdminRequest::OnlineBackup {
                config_path: path.to_str().ok_or("invalid_request")?.to_owned(),
                destination: destination.to_str().ok_or("invalid_request")?.to_owned(),
            };
            match notryx_server::admin::request(&config.server.admin_socket, &request).await? {
                AdminResponse::Ok {
                    result: result @ AdminResult::BackupCreated { .. },
                } => print_json(&result)?,
                AdminResponse::Error { code } => return Err(admin_error(&code).into()),
                _ => return Err("invalid_admin_response".into()),
            }
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
