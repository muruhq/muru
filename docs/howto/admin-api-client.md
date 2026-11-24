# Writing a client of the admin API

Everything `muructl` does to the daemon goes through the admin API, the
gRPC service `muru.admin.v1` on the admin socket. A program of your own
can call it too, from stubs generated from the `.proto` files of the
release it talks to. `muructl` carries those files and writes them out,
so the stubs always come from the release installed, with no network and
no source tree.

## Writing the files out

```console
$ muructl proto export protos
muru: wrote the .proto files of muructl 1.0.0 beneath protos
```

The files are laid out by package beneath the directory, so `protoc -I
protos` resolves their imports:

```
protos/muru/admin/v1/admin.proto
```

Only the admin API is written: the protocols between Muru's own
processes are not part of it. Each file starts with three comment lines
recording the muructl it came from and a hash of the rest, which `muructl
proto status` reads. The rest is the file the binary was built from.

`muructl proto list` names the files with the package and services of
each, and `--json` gives the same as JSON. `muructl proto print` writes
one file to standard output as `export` writes it, by default
`muru/admin/v1/admin.proto`.

## Generating stubs

Stubs are generated with each language's own tools. For Python, with
`grpcio-tools` installed:

```console
$ python -m grpc_tools.protoc -I protos --python_out=stubs --grpc_python_out=stubs protos/muru/admin/v1/admin.proto
```

which writes `stubs/muru/admin/v1/admin_pb2.py` and `admin_pb2_grpc.py`.
The generated modules are in a `muru.admin.v1` package, so keep them out
of an environment that also has the `muru` package installed, or
generate them into a package of your own.

## Calling the socket

`muructl status --json` names the socket the commands reach, as `socket`.
The daemon's, `muructl run`'s and `muructl serve`'s all serve the same
API; a development run and `muructl serve` answer the calls that read
and refuse those that need the daemon, such as deploying a release.
Whoever can open the socket can use the API, so a client runs as a user
the socket admits, as `muructl` does.

With grpcio, a client over a Unix socket sets the authority it sends,
since the one grpcio makes up from the socket's path is refused:

```python
import grpc

from muru.admin.v1 import admin_pb2, admin_pb2_grpc

channel = grpc.insecure_channel(
    "unix:/run/muru/admin.sock",
    options=[("grpc.default_authority", "localhost")],
)
admin = admin_pb2_grpc.AdminStub(channel)
print(admin.GetStatus(admin_pb2.GetStatusRequest()).version)
```

## Calling it by hand

`--descriptor-set FILE` also writes a compiled `FileDescriptorSet` of the
files, with their imports, which tools that read one take in place of the
files:

```console
$ muructl proto export protos --descriptor-set admin.protoset
$ grpcurl -plaintext -unix -protoset admin.protoset /run/muru/admin.sock muru.admin.v1.Admin/GetStatus
```

`buf` and a gRPC-JSON transcoder take the same file.

## Keeping the files current

The API changes between alpha releases, so the files and the stubs are
regenerated with every upgrade of the `muructl` a client pins. A client
that commits them can check in CI that they match the `muructl`
installed:

```console
$ muructl proto status protos
muru: the .proto files beneath protos are from muructl 1.0.0; this is muructl 1.1.0, and `muructl proto export protos` updates them
```

It says whether the files match, are from another release, or have
local changes, and exits 1 unless they match. `--json` gives `state`
(`current`, `missing`, `out_of_date`, `edited` or `edited_out_of_date`),
the two versions and each file's own state.
