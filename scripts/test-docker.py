#!/usr/bin/env python3
import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid


class CheckFailure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CheckFailure(message)


def docker(*arguments, check=True, timeout=60):
    result = subprocess.run(
        ["docker", *map(str, arguments)], capture_output=True, text=True, timeout=timeout
    )
    if check and result.returncode:
        raise CheckFailure(f"Docker {arguments[0]} failed")
    return result


def operation_id():
    value = bytearray(os.urandom(16))
    value[:6] = int(time.time() * 1000).to_bytes(6, "big")
    value[6] = (value[6] & 15) | 112
    value[8] = (value[8] & 63) | 128
    return str(uuid.UUID(bytes=bytes(value)))


def content_digest(data):
    return "sha-256=:" + base64.b64encode(hashlib.sha256(data).digest()).decode() + ":"


def container_snapshot():
    identifiers = docker("ps", "-q").stdout.split()
    if not identifiers:
        return {}
    result = docker(
        "inspect", "--format",
        "{{.Id}} {{.State.StartedAt}} {{.RestartCount}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}",
        *identifiers,
    )
    return {line.split()[0]: line.split()[1:] for line in result.stdout.splitlines()}


class Case:
    def __init__(self, image, prefix, label):
        self.image = image
        self.prefix = prefix
        self.label = label
        self.directory = tempfile.TemporaryDirectory(prefix=f"{prefix}-{label}-")
        self.root = Path(self.directory.name)
        self.notes = self.root / "notes"
        self.state = self.root / "state"
        self.notes.mkdir(mode=0o700)
        self.state.mkdir(mode=0o700)
        self.config = self.root / "server.toml"
        self.config.write_text(
            '[server]\nlisten = "0.0.0.0:8080"\nadmin_socket = "/run/mdw/admin.sock"\n'
            '[workspace]\nnotes_root = "/data/notes"\nstate_root = "/data/state"\n'
        )
        self.config.chmod(0o644)
        self.names = []
        self.name = None
        self.port = None
        self.paired = None
        self.info = None
        self.sequence = 0

    def __enter__(self):
        return self

    def __exit__(self, *_):
        for name in reversed(self.names):
            docker("rm", "--force", name, check=False)
        self.notes.chmod(0o700)
        self.directory.cleanup()

    def options(self, readonly=False, memory_state=False, missing_notes=False):
        uid, gid = os.getuid(), os.getgid()
        notes = self.root / "missing" if missing_notes else self.notes
        options = [
            "--pull", "never", "--init", "--user", f"{uid}:{gid}",
            "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
            "--memory", "1g", "--cpus", "2", "--pids-limit", "256",
            "--label", f"org.notryx.m1-test={self.prefix}",
            "--tmpfs", "/tmp:rw,noexec,nosuid,nodev,size=64m,mode=1777",
            "--tmpfs", f"/run/mdw:rw,noexec,nosuid,nodev,size=8m,mode=0700,uid={uid},gid={gid}",
            "--mount", f"type=bind,src={notes},dst=/data/notes" + (",readonly" if readonly else ""),
            "--mount", f"type=bind,src={self.config},dst=/etc/notryx/server.toml,readonly",
            "--log-driver", "json-file", "--log-opt", "max-size=1m", "--log-opt", "max-file=1",
        ]
        if memory_state:
            options += ["--tmpfs", f"/data/state:rw,noexec,nosuid,nodev,size=16m,mode=0700,uid={uid},gid={gid}"]
        else:
            options += ["--mount", f"type=bind,src={self.state},dst=/data/state"]
        return options

    def initialize(self):
        name = f"{self.prefix}-{self.label}-init"
        docker("create", "--name", name, "--network", "none", *self.options(), self.image,
               "init", "--config", "/etc/notryx/server.toml")
        self.names.append(name)
        docker("start", name)
        require(docker("wait", name).stdout.strip() == "0", "workspace initialization failed")

    def start(self, *, readonly=False, memory_state=False, expect_ready=True):
        name = f"{self.prefix}-{self.label}-{len(self.names)}"
        options = self.options(readonly=readonly, memory_state=memory_state)
        if memory_state:
            options += ["--entrypoint", "/bin/sh"]
            command = ["-ec", "/app/notryx-server init --config /etc/notryx/server.toml && exec /app/notryx-server serve --config /etc/notryx/server.toml"]
        else:
            command = ["serve", "--config", "/etc/notryx/server.toml"]
        docker("create", "--name", name, "--publish", "127.0.0.1::8080",
               *options, self.image, *command)
        self.names.append(name)
        self.name = name
        docker("start", name)
        if expect_ready:
            self.refresh_port()
            self.wait_ready()

    def refresh_port(self):
        published = docker("port", self.name, "8080/tcp").stdout.strip()
        require(published.startswith("127.0.0.1:"), "test port is not bound to loopback")
        self.port = int(published.rsplit(":", 1)[1])

    def wait_ready(self):
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            try:
                if self.request("GET", "/health/ready", auth=False)[0] == 200:
                    result = docker("exec", self.name, "/app/notryx-server", "healthcheck",
                                    "--address", "127.0.0.1:8080", check=False)
                    require(result.returncode == 0, "container healthcheck failed")
                    return
            except (OSError, http.client.HTTPException):
                pass
            running = docker("inspect", "--format", "{{.State.Running}}", self.name).stdout.strip()
            require(running == "true", "server exited before readiness")
            time.sleep(0.1)
        raise CheckFailure("server readiness timed out")

    def wait_failed(self):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            state = docker("inspect", "--format", "{{.State.Running}} {{.State.ExitCode}}", self.name).stdout.split()
            if state[0] == "false":
                require(state[1] != "0", "invalid storage was accepted")
                return
            time.sleep(0.1)
        raise CheckFailure("invalid storage did not stop startup")

    def request(self, method, path, *, data=None, headers=None, auth=True, expected=None):
        headers = dict(headers or {})
        if auth:
            headers["Authorization"] = f"Bearer {self.paired['device_token']}"
        if isinstance(data, dict):
            data = json.dumps(data).encode()
            headers["Content-Type"] = "application/json"
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=15)
        try:
            connection.request(method, path, data, headers)
            response = connection.getresponse()
            result = response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()
        if expected is not None:
            require(result[0] == expected, f"{method} {path} returned {result[0]}, expected {expected}")
        return result

    def admin(self, *arguments):
        output = docker("exec", self.name, "/app/notryx-server", "admin", "--config",
                        "/etc/notryx/server.toml", *arguments)
        return json.loads(output.stdout)

    def pair(self):
        status = self.admin("status")
        grant = self.admin("issue-pairing", status["owner_principal_id"])
        _, _, body = self.request("POST", "/v1/pair", auth=False, expected=200, data={
            "protocol": "mdw-sync/1.2", "workspace_id": grant["workspace_id"],
            "pairing_secret": grant["pairing_secret"],
        })
        self.paired = json.loads(body)
        self.info = json.loads(self.request("GET", "/v1/server-info", expected=200)[2])

    def identity(self, identifier):
        return {
            "X-Workspace-Epoch": self.info["workspace_epoch"],
            "X-Device-Stream": self.paired["device_stream_id"],
            "X-Operation-Seq": str(self.sequence), "Idempotency-Key": identifier,
        }

    def create(self, name):
        self.sequence += 1
        node = operation_id()
        self.request("POST", "/v1/nodes", expected=200, headers=self.identity(operation_id()), data={
            "node_id": node, "parent_id": self.info["root_id"], "name": name, "kind": "file",
        })
        return node

    def upload(self, data, expected=201):
        return self.request("POST", "/v1/uploads", data=data, expected=expected, headers={
            "Content-Type": "application/octet-stream", "Content-Digest": content_digest(data),
        })

    def prepared_save(self, node, data):
        _, headers, _ = self.request("GET", f"/v1/nodes/{node}/content", expected=200)
        self.sequence += 1
        identifier = operation_id()
        mutation = self.identity(identifier)
        mutation["If-Match"] = next(value for key, value in headers.items() if key.lower() == "etag")
        mutation["Content-Digest"] = content_digest(data)
        return identifier, mutation


def verify_normal(image, prefix):
    with Case(image, prefix, "normal") as case:
        case.initialize()
        case.start()
        require(docker("inspect", "--format", "{{.HostConfig.ReadonlyRootfs}}", case.name).stdout.strip() == "true",
                "container root filesystem was not configured readonly")
        require(docker("exec", case.name, "id", "-u").stdout.strip() == str(os.getuid()), "container UID mismatch")
        require(docker("exec", case.name, "id", "-g").stdout.strip() == str(os.getgid()), "container GID mismatch")
        require(docker("exec", "--user", "65534:65534", case.name, "/bin/sh", "-c",
                       "test -r /etc/notryx/server.toml", check=False).returncode == 0,
                "different UID could not read the synthetic configuration")
        denied = docker("exec", "--user", "65534:65534", case.name, "/app/notryx-server", "admin",
                        "--config", "/etc/notryx/server.toml", "status", check=False)
        require(denied.returncode != 0, "different UID could access the administrator socket")
        require(docker("exec", case.name, "/bin/sh", "-c", "touch /app/forbidden", check=False).returncode != 0,
                "container root filesystem is writable")
        case.pair()
        node = case.create("Docker.md")
        payload = b"\xef\xbb\xbf# Synthetic Docker fixture\r\n"
        identifier, headers = case.prepared_save(node, payload)
        path = f"/v1/nodes/{node}/content"
        original_receipt = case.request("PUT", path, data=payload, headers=headers, expected=200)[2]
        require((case.notes / "Docker.md").read_bytes() == payload, "host did not observe committed bytes")
        require((case.notes / "Docker.md").stat().st_uid == os.getuid(), "new file UID mismatch")
        (case.notes / "host-created.md").write_bytes(b"host tool fixture\n")
        docker("exec", case.name, "/bin/sh", "-c", "printf 'container write\\n' >> /data/notes/host-created.md")
        require((case.notes / "host-created.md").read_bytes() == b"host tool fixture\ncontainer write\n",
                "container could not update a host-owned file")
        docker("kill", "--signal", "KILL", case.name)
        docker("start", case.name)
        case.refresh_port()
        case.wait_ready()
        require(case.admin("status")["workspace_id"] == case.info["workspace_id"], "administrator socket was not recreated")
        require(case.request("GET", path, expected=200)[2] == payload, "restart changed source bytes")
        require(case.request("PUT", path, data=payload, headers=headers, expected=200)[2] == original_receipt,
                "restart changed an existing operation receipt")
        operation = json.loads(case.request("GET", f"/v1/operations/{identifier}", expected=200)[2])
        require(operation["status"] == "committed", "saved operation was not preserved")


def verify_upload(image, prefix):
    with Case(image, prefix, "upload") as case:
        case.initialize()
        case.start()
        case.pair()
        payload = bytes(range(256)) * (5 * 4096) + b"\0\xff\r\n"
        staged = json.loads(case.upload(payload)[2])
        require(staged["content_hash"] == "sha256:" + hashlib.sha256(payload).hexdigest(),
                "upload digest did not describe the exact source bytes")
        require(staged["size"] == str(len(payload)), "upload size was incorrect")
        require(not (case.notes / "Attachment.bin").exists(), "upload published a node without a mutation")
        docker("kill", "--signal", "KILL", case.name)
        docker("start", case.name)
        case.refresh_port()
        case.wait_ready()
        case.sequence += 1
        node, identifier = operation_id(), operation_id()
        headers = case.identity(identifier)
        body = {"node_id": node, "parent_id": case.info["root_id"], "name": "Attachment.bin",
                "kind": "file", "upload_id": staged["upload_id"]}
        receipt = case.request("POST", "/v1/nodes", data=body, headers=headers, expected=200)[2]
        require((case.notes / "Attachment.bin").read_bytes() == payload, "attachment creation changed source bytes")
        docker("stop", "--time", "30", case.name)
        docker("start", case.name)
        case.refresh_port()
        case.wait_ready()
        require(case.request("POST", "/v1/nodes", data=body, headers=headers, expected=200)[2] == receipt,
                "consumed upload could not replay its original receipt after restart")
        require(case.request("GET", f"/v1/nodes/{node}/content", expected=200)[2] == payload,
                "attachment did not survive restart")
        case.sequence += 1
        body.update(node_id=operation_id(), name="Reused.bin")
        case.request("POST", "/v1/nodes", data=body, headers=case.identity(operation_id()), expected=410)
        require(not (case.notes / "Reused.bin").exists(), "consumed upload was attached twice")


def verify_startup_guards(image, prefix):
    with Case(image, prefix, "empty") as case:
        case.start(expect_ready=False)
        case.wait_failed()
        require(not any(case.notes.iterdir()) and not any(case.state.iterdir()), "startup initialized empty mounts")
        missing_name = f"{prefix}-missing-bind"
        rejected = docker("create", "--name", missing_name, *case.options(missing_notes=True), image,
                          "serve", "--config", "/etc/notryx/server.toml", check=False)
        if rejected.returncode == 0:
            case.names.append(missing_name)
        require(rejected.returncode != 0 and not (case.root / "missing").exists(), "missing bind path was created")
    with Case(image, prefix, "marker") as case:
        case.initialize()
        marker = case.notes / ".mdw-workspace.json"
        value = json.loads(marker.read_bytes())
        value["workspace_id"] = str(uuid.uuid4())
        marker.write_text(json.dumps(value))
        observed = marker.read_bytes()
        case.start(expect_ready=False)
        case.wait_failed()
        require(marker.read_bytes() == observed, "startup rewrote a mismatched marker")


def verify_write_failure(image, prefix, readonly):
    label = "readonly" if readonly else "permission"
    with Case(image, prefix, label) as case:
        case.initialize()
        case.start()
        case.pair()
        node = case.create("Preserved.md")
        before = b"preserved fixture\r\n"
        _, initial = case.prepared_save(node, before)
        path = f"/v1/nodes/{node}/content"
        case.request("PUT", path, data=before, headers=initial, expected=200)
        docker("stop", "--time", "30", case.name)
        if not readonly:
            case.notes.chmod(0o500)
        case.start(readonly=readonly)
        identifier, headers = case.prepared_save(node, b"must not replace before")
        response = case.request("PUT", path, data=b"must not replace before", headers=headers)
        require(response[0] == 503, f"{label} failure was not reported as unavailable")
        require((case.notes / "Preserved.md").read_bytes() == before, f"{label} failure changed source bytes")
        docker("stop", "--time", "30", case.name)
        case.notes.chmod(0o700)
        case.start()
        require(case.request("GET", path, expected=200)[2] == before, f"{label} recovery lost the source")
        case.request("GET", f"/v1/operations/{identifier}", expected=404)
        retried = b"must not replace before"
        case.request("PUT", path, data=retried, headers=headers, expected=200)
        result = json.loads(case.request("GET", f"/v1/operations/{identifier}", expected=200)[2])
        require(result["status"] == "committed", f"{label} failure consumed the operation before admission")
        require((case.notes / "Preserved.md").read_bytes() == retried, f"{label} recovery did not apply the retry")


def verify_disk_full(image, prefix):
    with Case(image, prefix, "diskfull") as case:
        case.start(memory_state=True)
        case.pair()
        node = case.create("Preserved.md")
        before = b"committed before disk full\n"
        _, initial = case.prepared_save(node, before)
        path = f"/v1/nodes/{node}/content"
        case.request("PUT", path, data=before, headers=initial, expected=200)
        identifier, headers = case.prepared_save(node, b"write after freeing space")
        filled = docker("exec", "--env", "LC_ALL=C", case.name, "/bin/sh", "-c",
                        "dd if=/dev/zero of=/data/state/.mdw-test-fill bs=65536 count=512", check=False)
        require(filled.returncode != 0 and "No space left on device" in filled.stderr, "bounded tmpfs did not reach ENOSPC")
        rejected_upload = case.upload(b"upload during disk full", expected=507)
        require(json.loads(rejected_upload[2])["code"] == "insufficient_storage", "ENOSPC upload error was misclassified")
        rejected_save = case.request("PUT", path, data=b"write after freeing space", headers=headers, expected=507)
        require(json.loads(rejected_save[2])["code"] == "insufficient_storage", "ENOSPC save error was misclassified")
        require((case.notes / "Preserved.md").read_bytes() == before, "ENOSPC changed committed source bytes")
        require(case.request("GET", path, expected=200)[2] == before, "ENOSPC blocked committed content reads")
        docker("exec", case.name, "rm", "/data/state/.mdw-test-fill")
        case.request("PUT", path, data=b"write after freeing space", headers=headers, expected=200)
        result = json.loads(case.request("GET", f"/v1/operations/{identifier}", expected=200)[2])
        require(result["status"] == "committed", "ENOSPC consumed a new operation before admission")
        case.upload(b"upload after freeing space")


def main():
    parser = argparse.ArgumentParser(description="격리된 Linux Docker 컨테이너로 M1 저장과 배포 경계를 검증합니다.")
    parser.add_argument("--image", required=True, help="미리 빌드한 로컬 서버 이미지")
    parser.add_argument("--output", type=Path, help="credential을 제외한 결과 JSON 경로")
    arguments = parser.parse_args()
    require(sys.platform.startswith("linux") and os.getuid() != 0, "run as a nonroot Linux user")
    endpoint = docker("context", "inspect", "--format", "{{.Endpoints.docker.Host}}").stdout.strip()
    require(endpoint.startswith("unix://"), "tests require a local Docker socket")
    docker("image", "inspect", arguments.image, "--format", "{{.Id}}")
    before = container_snapshot()
    prefix = "notryx-m1-" + uuid.uuid4().hex[:12]
    results = []
    checks = [
        ("runtime_uid_socket_restart", lambda: verify_normal(arguments.image, prefix)),
        ("upload_attachment_and_receipt_restart", lambda: verify_upload(arguments.image, prefix)),
        ("empty_missing_and_mismatched_mounts", lambda: verify_startup_guards(arguments.image, prefix)),
        ("actual_permission_failure", lambda: verify_write_failure(arguments.image, prefix, False)),
        ("actual_readonly_mount", lambda: verify_write_failure(arguments.image, prefix, True)),
        ("actual_enospc", lambda: verify_disk_full(arguments.image, prefix)),
    ]
    try:
        for name, check in checks:
            started = time.monotonic()
            check()
            results.append({"check": name, "status": "passed", "seconds": round(time.monotonic() - started, 3)})
            print(f"PASS {name}", flush=True)
    finally:
        remaining = docker("ps", "-aq", "--filter", f"label=org.notryx.m1-test={prefix}").stdout.split()
        require(not remaining, "a test container was not removed")
        after = container_snapshot()
        require(all(after.get(key) == value for key, value in before.items()), "a pre-existing container changed its running or health state during tests")
        if arguments.output:
            arguments.output.parent.mkdir(parents=True, exist_ok=True)
            arguments.output.write_text(json.dumps({
                "complete": len(results) == len(checks),
                "checks": results,
                "not_completed": [name for name, _ in checks[len(results):]],
                "pre_existing_containers_preserved": True,
                "test_containers_removed": True,
            }, indent=2) + "\n")


if __name__ == "__main__":
    try:
        main()
    except CheckFailure as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
    except Exception as error:
        print(f"FAIL: unexpected {type(error).__name__}", file=sys.stderr)
        sys.exit(1)
