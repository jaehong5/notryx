#!/usr/bin/env python3
"""Run the synthetic manual credential restart scenario inside its sandbox."""

import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import select
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid


class Failure(Exception):
    pass


def require(value, message):
    if not value:
        raise Failure(message)


def operation_id():
    value = bytearray(os.urandom(16))
    value[:6] = int(time.time() * 1000).to_bytes(6, "big")
    value[6] = (value[6] & 15) | 112
    value[8] = (value[8] & 63) | 128
    return str(uuid.UUID(bytes=bytes(value)))


def prompt(message):
    print(message, flush=True)
    ready, _, _ = select.select([sys.stdin], [], [], 600)
    require(bool(ready), "사용자 입력 시간이 초과됐습니다.")
    require(sys.stdin.readline() != "", "터미널 입력이 종료됐습니다.")


def wait_for(check, message, seconds=90):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(0.2)
    raise Failure(message)


def leader_running(process):
    if process.returncode is not None:
        return False
    try:
        return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None
    except ChildProcessError:
        raise Failure("시험 프로세스의 소유권을 확인하지 못했습니다.") from None


def group_running(group):
    for path in Path("/proc").iterdir():
        if not path.name.isdecimal():
            continue
        try:
            stat = (path / "stat").read_text()
        except FileNotFoundError:
            continue
        except OSError:
            raise Failure("시험 프로세스 그룹을 확인하지 못했습니다.") from None
        fields = stat[stat.rfind(")") + 2:].split()
        require(len(fields) > 2, "시험 프로세스 상태를 확인하지 못했습니다.")
        if int(fields[2]) == group and fields[0] not in ("Z", "X"):
            return True
    return False


def stop(process):
    if process is None or process.returncode is not None:
        return
    leader_running(process)
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 10
    while group_running(process.pid) and time.monotonic() < deadline:
        time.sleep(0.1)
    if group_running(process.pid):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 10
        while group_running(process.pid) and time.monotonic() < deadline:
            time.sleep(0.1)
    require(not group_running(process.pid), "시험 프로세스 그룹 종료를 확인하지 못했습니다.")
    process.wait(timeout=10)


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--server", required=True)
    args = parser.parse_args()
    require(sys.stdin.isatty() and sys.stdout.isatty(), "수동 시험은 직접 연결된 터미널이 필요합니다.")
    require(os.environ.get("NOTRYX_TEST_SANDBOX") == "credential-manual", "시험 격리 표시가 없습니다.")
    bus = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    require(bus == "unix:path=/work/run/bus.sock" or bus.startswith("unix:path=/work/run/bus.sock,"), "독립 D-Bus가 아닙니다.")
    require(os.environ.get("TMPDIR") == "/work/tmp", "시험 임시 디렉터리가 아닙니다.")
    app_binary = str(Path(args.binary).resolve(strict=True))
    server_binary = str(Path(args.server).resolve(strict=True))
    require(app_binary == "/work/notryx-client" and server_binary == "/work/notryx-server", "시험 바이너리 경로가 아닙니다.")
    app = server = None
    with tempfile.TemporaryDirectory(prefix="notryx-manual-") as temp:
        root = Path(temp)
        notes, state, run, client, xdg_data, xdg_cache, xdg_config = (
            root / name for name in ("notes", "state", "run", "client", "xdg-data", "xdg-cache", "xdg-config")
        )
        for path in (notes, state, run, client, xdg_data, xdg_cache, xdg_config):
            path.mkdir(mode=0o700)
        port = free_port()
        origin = f"http://127.0.0.1:{port}"
        config = root / "server.toml"
        config.write_text(
            f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n'
            f'[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8"
        )
        config.chmod(0o600)
        database = client / "drafts.sqlite3"

        def cli(*arguments):
            try:
                return subprocess.run([server_binary, *arguments], stdout=subprocess.PIPE,
                                      stderr=subprocess.DEVNULL, text=True, timeout=30, check=True).stdout
            except (OSError, subprocess.SubprocessError):
                raise Failure("합성 서버 명령이 실패했습니다.") from None

        def admin(*arguments):
            return json.loads(cli("admin", "--config", str(config), *arguments))

        def request(method, path, body=None, headers=None, expected=200):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
            try:
                connection.request(method, path, body, headers or {})
                response = connection.getresponse()
                data = response.read(1024 * 1024 + 1)
                require(response.status == expected and len(data) <= 1024 * 1024, "합성 서버 응답을 확인하지 못했습니다.")
                return json.loads(data) if data else None
            except (OSError, http.client.HTTPException, ValueError):
                raise Failure("합성 서버 요청이 실패했습니다.") from None
            finally:
                connection.close()

        def start_server():
            process = subprocess.Popen([server_binary, "serve", "--config", str(config)],
                                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                       text=True, start_new_session=True)
            ready, _, _ = select.select([process.stdout], [], [], 30)
            if not ready or process.stdout.readline().strip() != "server_started" or not leader_running(process):
                stop(process)
                raise Failure("합성 서버가 시작되지 않았습니다.")
            return process

        def start_app():
            environment = os.environ.copy()
            environment.update(NOTRYX_M0_DATA_DIR=str(client), XDG_DATA_HOME=str(xdg_data),
                               XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config))
            return subprocess.Popen([app_binary], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, env=environment, start_new_session=True)

        def active_binding():
            if not database.exists():
                return None
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id,l.item_id
                    FROM client_registrations r JOIN credential_links l ON l.registration_id=r.id
                    WHERE r.status='active' AND l.item_id IS NOT NULL
                """).fetchone()

        def outbox(node_id):
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT o.operation_id,o.operation_seq,o.status,o.bytes,c.bytes,b.bytes
                    FROM client_outbox o
                    JOIN client_registrations r ON r.id=o.registration_id
                    JOIN registered_documents d ON d.registration_id=r.id AND d.document_id=o.document_id
                    JOIN draft_checkpoints c ON c.document_id=d.storage_id
                    JOIN draft_bases b ON b.document_id=d.storage_id
                    WHERE r.status='active' AND o.document_id=? AND o.operation_kind='content'
                    ORDER BY o.operation_seq
                """, (node_id,)).fetchall()

        try:
            cli("init", "--config", str(config))
            server = start_server()
            status = admin("status")
            grant = admin("issue-pairing", status["owner_principal_id"])
            owner = request("POST", "/v1/pair", json.dumps({
                "protocol": "mdw-sync/1.2", "workspace_id": grant["workspace_id"],
                "pairing_secret": grant["pairing_secret"],
            }).encode(), {"Content-Type": "application/json"})
            auth = {"Authorization": f"Bearer {owner['device_token']}"}
            info = request("GET", "/v1/server-info", headers=auth)
            node_id = operation_id()
            name = "시험 문서.md"
            original = "# 합성 시험 문서\n".encode()
            digest = base64.b64encode(hashlib.sha256(original).digest()).decode()
            upload = request("POST", "/v1/uploads", original, {
                **auth, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{digest}:"
            }, expected=201)
            request("POST", "/v1/nodes", json.dumps({
                "node_id": node_id, "parent_id": info["root_id"], "name": name,
                "kind": "file", "upload_id": upload["upload_id"],
            }).encode(), {
                **auth, "Content-Type": "application/json", "X-Workspace-Epoch": info["workspace_epoch"],
                "X-Device-Stream": owner["device_stream_id"], "X-Operation-Seq": "1",
                "Idempotency-Key": operation_id(),
            })
            ui_grant = admin("issue-pairing", status["owner_principal_id"])
            app = start_app()
            print("터미널 기록을 끄고 시험하세요. 다음 정보는 이번 임시 서버에서만 쓰는 합성 등록 정보입니다. 클립보드에 복사하지 말고 직접 입력하세요.", flush=True)
            print(f"서버 주소: {origin}\n작업 공간 ID: {ui_grant['workspace_id']}\n등록 코드: {ui_grant['pairing_secret']}", flush=True)
            prompt("앱에 등록한 뒤 '시험 문서.md'를 열고 터미널에서 Enter를 누르세요.")
            require(leader_running(app), "시험 앱이 종료됐습니다.")
            binding = wait_for(active_binding, "보안 저장된 등록을 확인하지 못했습니다.")
            require(binding[0] == origin, "등록 서버가 합성 서버와 다릅니다.")
            stop(server)
            server = None
            prompt("합성 서버가 중지됐습니다. 열린 문서에 개인 자료가 없는 합성 시험 문장을 입력하고 저장한 뒤 Enter를 누르세요.")
            require(leader_running(app), "시험 앱이 종료됐습니다.")
            pending = wait_for(lambda: outbox(node_id), "기존 전송 대기 작업을 확인하지 못했습니다.")
            require(len(pending) == 1 and pending[0][2] in ("queued", "sent"), "전송 대기 작업 상태가 다릅니다.")
            operation, sequence, _, payload, draft, base = pending[0]
            require(payload == draft and payload != original and base == original, "초안, 기준 본문 또는 전송 bytes가 다릅니다.")
            prompt("앱을 완전히 닫고 터미널에서 Enter를 누르세요.")
            require(not leader_running(app), "시험 앱이 아직 실행 중입니다.")
            stop(app)
            require(active_binding() == binding, "앱 종료 후 등록이 달라졌습니다.")
            closed = outbox(node_id)
            require(len(closed) == 1 and (closed[0][0], closed[0][1], closed[0][3:]) ==
                    (operation, sequence, pending[0][3:]), "앱 종료 후 초안 또는 대기 작업이 달라졌습니다.")
            app = start_app()
            prompt("서버가 꺼진 상태로 앱을 다시 열었습니다. 시험 문서의 내용이 복구됐는지 확인하고 Enter를 누르세요. 시험이 끝날 때까지 앱을 닫거나 추가 저장, 재등록을 누르지 마세요.")
            require(leader_running(app) and active_binding() == binding, "재시작된 앱 또는 등록을 확인하지 못했습니다.")
            restarted = outbox(node_id)
            require(len(restarted) == 1 and (restarted[0][0], restarted[0][1], restarted[0][3:]) ==
                    (operation, sequence, pending[0][3:]), "재시작 후 초안 또는 대기 작업이 달라졌습니다.")
            require((notes / name).read_bytes() == original, "오프라인 중 서버 원문이 달라졌습니다.")
            server = start_server()
            print("합성 서버를 다시 시작했습니다. 추가 등록이나 저장 없이 자동 전송을 기다립니다.", flush=True)
            wait_for(lambda: (notes / name).read_bytes() == payload, "서버 본문이 자동으로 갱신되지 않았습니다.")

            def confirmed():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                    row = db.execute("""
                        SELECT s.confirmed_through FROM outbox_streams s JOIN client_registrations r
                        ON r.id=s.registration_id WHERE r.status='active'
                    """).fetchone()
                    return row is not None and row[0] >= sequence

            wait_for(confirmed, "클라이언트 ACK 확인이 완료되지 않았습니다.")
            require(active_binding() == binding and leader_running(app), "전송 후 등록 또는 앱 상태가 달라졌습니다.")
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                changes = db.execute("SELECT COUNT(*) FROM change_sets WHERE operation_id=?", (operation,)).fetchone()[0]
                retired = db.execute("SELECT COUNT(*) FROM retired_operations WHERE operation_id=? AND stream_id=? AND operation_seq=?", (operation, binding[4], sequence)).fetchone()[0]
                stream = db.execute("SELECT last_seq,retired_through FROM operation_streams WHERE stream_id=?", (binding[4],)).fetchone()
            require(changes == 1 and retired == 1 and stream is not None and int(stream[0]) >= sequence and int(stream[1]) >= sequence,
                    "서버 작업 이력 또는 ACK가 일치하지 않습니다.")
            print("합성 인증정보 복원과 기존 전송 대기 작업의 자동 반영 및 ACK를 확인했습니다.", flush=True)
        finally:
            try:
                stop(app)
            finally:
                stop(server)


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"수동 시험 실패: {failure}", file=sys.stderr, flush=True)
        raise SystemExit(1) from None
    except Exception:
        print("수동 시험 실패: 시험 환경 오류", file=sys.stderr, flush=True)
        raise SystemExit(1) from None
