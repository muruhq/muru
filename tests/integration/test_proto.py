"""The admin API's `.proto` files given out by `muructl proto` (issue 858):
a client builds from the files the binary was built from, and a copy is
checked against the muructl in use."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from support import server as support


def muructl(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    path = support.muructl_path()

    if path is None:
        pytest.skip("muructl must be built")

    return subprocess.run(
        [str(path), *arguments],
        capture_output=True,
        text=True,
        check=check,
        timeout=60,
    )


def test_a_client_built_from_the_exported_files_calls_a_run(tmp_path: Path) -> None:
    grpc = pytest.importorskip("grpc")
    protoc = pytest.importorskip("grpc_tools.protoc")
    from google.protobuf import descriptor_pb2, descriptor_pool, message_factory

    if not support.extension_available():
        pytest.skip("the extension must be built")

    protos = tmp_path / "protos"
    descriptor_set = tmp_path / "admin.protoset"

    # Nothing is there yet, and status says so with exit status 1.

    missing = muructl("proto", "status", str(protos), "--json", check=False)
    assert missing.returncode == 1
    assert json.loads(missing.stdout)["state"] == "missing"

    muructl("proto", "export", str(protos), "--descriptor-set", str(descriptor_set))
    assert muructl("proto", "status", str(protos)).returncode == 0

    # The files compile with the language's own tools, as a client's
    # stubs are generated.

    stubs = tmp_path / "stubs"
    stubs.mkdir()
    admin = protos / "muru" / "admin" / "v1" / "admin.proto"
    arguments = ["protoc", f"-I{protos}", f"--python_out={stubs}", f"--grpc_python_out={stubs}"]
    assert protoc.main([*arguments, str(admin)]) == 0
    assert (stubs / "muru" / "admin" / "v1" / "admin_pb2_grpc.py").is_file()

    # The descriptor set describes the same service, and a call made from
    # it reaches a development run's admin socket.

    files = descriptor_pb2.FileDescriptorSet.FromString(descriptor_set.read_bytes())
    pool = descriptor_pool.DescriptorPool()

    for file in files.file:
        pool.Add(file)

    request = message_factory.GetMessageClass(
        pool.FindMessageTypeByName("muru.admin.v1.GetStatusRequest")
    )
    answer = message_factory.GetMessageClass(pool.FindMessageTypeByName("muru.admin.v1.Status"))
    runtime = support.short_dir("muru-rt-")
    server = support.start("plain_wsgi:application", env={"MURU_RUN_DIR": str(runtime)})

    try:
        with grpc.insecure_channel(
            f"unix:{server.admin_socket()}", options=[("grpc.default_authority", "localhost")]
        ) as channel:
            get_status = channel.unary_unary(
                "/muru.admin.v1.Admin/GetStatus",
                request_serializer=request.SerializeToString,
                response_deserializer=answer.FromString,
            )
            status = get_status(request(), timeout=10)

        assert status.kind == "run" and status.app == "plain-wsgi", status
    finally:
        server.stop()

    # A change to a file written is a local change.

    admin.write_text(admin.read_text() + "// ours\n")
    edited = muructl("proto", "status", str(protos), "--json", check=False)
    assert edited.returncode == 1
    assert json.loads(edited.stdout)["state"] == "edited"
