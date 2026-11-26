# Commands and files

What a daemon serves can be changed by command, from files, or by both
on different applications. This guide shows each way of working, how to
move from one to the other, and what to expect where they meet.
[Three kinds of configuration](../explanation/configuration.md)
describes what the files are, and [the daemon](daemon.md) each command.

## By command

No files are needed. `muructl deploy` makes a release of a project and
moves traffic to it, and `muructl app set` keeps what belongs to this
node on the application, where every later release takes it:

```console
$ muructl daemon start
$ muructl app set shop --env SHOP_DATABASE=postgres://db/shop --processes 4
$ muructl deploy shop /srv/shop
$ muructl rollback shop
```

`muructl rollout` moves traffic between releases by steps or at once,
and `muructl release prune` removes old releases.

## From files

A file declares applications, their releases, hosts and routes, and
`muructl apply` makes the daemon match it:

```toml
# deploy.d/shop.toml
[app.shop]
processes = 4

[app.shop.env]
SHOP_DATABASE = "postgres://db/shop"

[app.shop.release.2026-11-06]
source = "/srv/shop/2026-11-06"

[host.shop]
names = ["shop.example.com"]

[[route]]
host = "shop"
app = "shop"
```

```console
$ muructl apply -f deploy.d/ --dry-run
$ muructl apply -f deploy.d/
```

A new release is a new table. `muructl render` writes one from the
command line `muructl deploy` takes, under an id of your choosing:

```console
$ muructl render shop /srv/shop/2026-11-12 --release 2026-11-12 > deploy.d/shop-2026-11-12.toml
$ muructl apply -f deploy.d/
```

An apply that adds one release of an application gives it the traffic.
One that adds several needs the application's `serving` key to say
which serves. To remove a release, take its table or its file away and
apply with `--prune`.

## Hosts and routes in files, the application by command

Hosts and routes are declared in files. The application they send
requests to need not be: leave it out of the files, and it stays yours
to release and set by command.

```toml
# site.toml
deployment = "site"

[host.shop]
names = ["shop.example.com"]

[[route]]
host = "shop"
app = "shop"
```

```console
$ muructl app set shop --processes 4
$ muructl deploy shop /srv/shop
$ muructl apply -f site.toml
```

The application is in the deployment `default`, and the file's
deployment owns the host and the route. `muructl apply -f site.toml
--prune` tidies that deployment and leaves the application alone.
`--prune=all` on such a node removes what no file declares, the
application among it.

## Where the two meet

An application that a file declares gets its releases and its settings
from the files. Acting on it by command still works where it has to,
and says what the files will do:

| By command | On an application the files declare |
| --- | --- |
| `muructl deploy` | Works, for an emergency. It warns that the files do not have the release. When the files name `serving`, their next apply moves traffic back |
| `muructl rollback` | Works. An apply whose `serving` still names the release rolled back from is refused, until the key names another release or that release is promoted again |
| `muructl rollout bluegreen` | Works. An apply that moves traffic back says that a command chose the release |
| `muructl app set` | Refused in a named deployment. In `default` it works and says that the files' next apply removes what they do not carry |
| `muructl release remove` | Refused for a release the files declare, unless `--force` |

After an emergency deploy, bring the release into the files: `muructl
deployment export` writes the serving release with every key.

## Moving an application from commands to files

An application made by command is in `default`. Write it out as a
deployment of its own, look at what applying it changes, and apply it:

```console
$ muructl deployment export --app shop --as shop > deploy.d/shop.toml
$ muructl apply -f deploy.d/shop.toml --dry-run
muru: no changes (4 objects as declared)
muru: app shop moves from the deployment default to shop
$ muructl apply -f deploy.d/shop.toml
```

The file carries the settings `muructl app set` gave, the release that
serves and the `serving` key naming it, and the hosts and routes that
reach the application. A release that neither serves nor is named by a
route's traffic is not written, and stays until it is removed. A
rollout under way is not part of the file: let it end first, or write
it as where it stands with `--resolve-rollouts`.

From then on the file decides which release serves, and its settings
are the file's. The first `--prune` removes the releases the file does
not declare, leaving the one that serves and the one a rollback
restores.

## A node kept in version control

The repository holds the daemon's configuration and a directory of
deployment files:

```
node/
  muru.toml
  deploy.d/
    hosts.toml
    shop.toml
    blog.toml
```

```console
$ muructl daemon start --config node/muru.toml --apply node/deploy.d/
```

The daemon starts, applies the files, and is ready when they are
applied. Afterwards, change a file and apply it, or change `muru.toml`
and reload:

```console
$ muructl apply -f node/deploy.d/ --dry-run
$ muructl apply -f node/deploy.d/
$ muructl daemon config check
$ muructl daemon reload
```

`--apply` applies the files at every start. On a machine that is also
changed by hand, leave it off the service's command line and apply
yourself, since a restart then deploys whatever the files say at that
moment. With `--prune=all` beside it, the files are the whole of what
the daemon serves at each start.

## Several deployments on one node

A node that serves unrelated things can keep each as a deployment of
its own, each with its files in its own repository:

```toml
# /srv/shop/muru.deploy.toml
deployment = "shop"

[app.shop.release.2026-11-06]
source = "."

[host.shop]
names = ["shop.example.com"]

[[route]]
host = "shop"
app = "shop"
```

```console
$ muructl apply -f /srv/shop/muru.deploy.toml --prune
$ muructl apply -f /srv/blog/muru.deploy.toml --prune
```

Each apply changes and prunes only its own deployment.

Put together what you would remove together. Two applications that
make one site, released together, go in one deployment with the site's
host and routes. When they are released separately, the host goes in a
deployment of its own and each application's deployment holds its
routes on that host. The host is then not removed while either
application's routes name it.

A condition goes beside what it watches, in that deployment, and
another deployment's overlay may be triggered by it. Client classes
and rate limits that apply to every site go in the daemon's own file.

## A daemon in a container

The image carries the files, and one command brings up a daemon that
serves:

```console
$ muructl daemon run --config /etc/muru/muru.toml --apply /etc/muru/site.toml
```

```toml
# /etc/muru/site.toml
[app.site]
data_dir = "/data/site"

[app.site.release.r1]
source = "/app"
```

The application's data directory is in place before its first release
starts. A container whose files cannot be applied exits with the
reason.

## Starting again

To make a running node match a set of files, whatever it held, apply
them with `--prune=all`, which removes only what the files do not
declare. To put a daemon back to nothing, stop it and reset it:

```console
$ muructl daemon stop --shutdown
$ muructl daemon reset
$ muructl daemon start --apply node/deploy.d/
```

After a reset every release in the files is new to the daemon, so files
that declare more than one release of an application name the one that
serves with its `serving` key.
