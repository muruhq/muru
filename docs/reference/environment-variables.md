# Environment variables

The variables Muru sets for an application's host and workers, its
[commands](../howto/commands.md) and `muructl exec`. Muru alone sets
them: a variable of the same name in `[tool.muru.env]`, from `muructl
deploy --env` or in the shell that started `muructl` is replaced, and a
place the application does not have is left unset rather than
inherited.

| Variable | Set to |
| --- | --- |
| `MURU_APP` | The application's name |
| `MURU_RELEASE` | The release, such as `r3`; not set for a command `muructl` runs itself for a project |
| `MURU_RELEASE_DIR` | The release's source directory, which is also the working directory |
| `MURU_DATA_DIR` | The application's [data directory](../howto/data-directory.md), shared by every release; not set under `muructl serve` without `--data-dir` |
| `MURU_MEDIA_DIR` | The data directory's media part, the one part ever served, by [file hints](../howto/file-hints.md) and `media:` static entries |
| `MURU_UPLOADS_DIR` | The data directory's uploads part, never served, where `upload.save()` puts a file by default |
| `MURU_STATIC_DIR` | The release's own directory of built static files, named `static:` in [static entries](../howto/static-files.md#built-files) |
| `MURU_COMMAND` | The declared command that runs, in a command |
| `MURU_GROUP` | The worker's group, in a worker |
| `MURU_LOG_LEVEL`, `MURU_LOG_FORMAT` | Python logging's defaults |

Where the data and static directories are depends on how the
application runs:

| How the application runs | `MURU_DATA_DIR` | `MURU_STATIC_DIR` |
| --- | --- | --- |
| The daemon | `STATE/data/APP`, or the application's `data_dir` | `STATE/static/APP/RELEASE` |
| `muructl run` | `runs/NAME-HASH/data` in the user state directory, or `--data-dir`, or the run table's or `muru.local.toml`'s `data_dir` | `runs/NAME-HASH/static` in the user state directory |
| `muructl serve` | `--data-dir`, `MURU_SERVE_DATA_DIR` or the serve table's `data_dir`; none otherwise | `staticfiles` in the project, or `--static-dir` |
| `muructl command run` and `muructl exec` with no application | `MURU_SERVE_DATA_DIR`, else as `muructl run` without `--data-dir` | `MURU_SERVE_STATIC_DIR`, else as `muructl run` |

`MURU_MEDIA_DIR` and `MURU_UPLOADS_DIR` are `media` and `uploads` in the
data directory unless the daemon's `media_dir` and `uploads_dir`, or
serve's `--media-dir` and `--uploads-dir`, move them.
