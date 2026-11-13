# Inspecting requests in development

`muructl run` keeps a record of every request it serves: its timing,
where it was routed, its response and headers, what the application
logged and the exception it raised. With `--toolbar` each HTML page
shows the record of the request that produced it, in a toolbar on the
page. The same record is looked up by the request's ID on the dashboard,
with `muructl debug request` and through the MCP server, so a JSON API's
requests can be inspected as easily as a page's, and every response
tells the browser's own developer tools how long it took.

All of this is part of development mode: under `--allow-public`,
`muructl serve` and the daemon, there is no toolbar, responses carry
neither header and no records are kept.

## The toolbar on the page

Turn the toolbar on for a run with `--toolbar`:

```console
$ muructl run shop.wsgi:application --toolbar
...
muru: toolbar on: the edge inserts it into HTML pages, with each request's record under /_muru/
```

or for every run of the project with `toolbar = true` in its
`[tool.muru.run]` table, or for your own checkout in `muru.local.toml`
(see [settings for muructl run](run-settings.md)). `--no-toolbar` turns a
file's setting off for one run.

Each HTML page then has a badge in its bottom right corner with the
request's status, its total and queue times, the worker that served it,
and how many log records and errors it had. Clicking the badge opens the
record's panels, the same ones the [dashboard's request
page](#a-requests-record) shows, as tabs, and "open in dashboard" opens
that page. A page whose response is still streaming updates as the
record fills in. The toolbar only shows: it has no forms and changes
nothing.

The application is not changed. Muru inserts one tag before the page's
`</body>`:

```html
<script src="/_muru/toolbar.js" data-request="01JCZ8X4R3T5V6W7Y8Z9A0B1C2" async></script>
```

It goes into a page a browser loads as a document, so `fetch()` and htmx
requests, which say otherwise in `Sec-Fetch-Dest`, are left as they are.
Muru never changes:

- a response to anything but GET, or one whose status is 204, 206 or
  304;
- a response that is not `text/html`, or has no `</body>`;
- a response the application encoded itself (`Content-Encoding`) or
  marked `Cache-Control: no-transform`;
- a file Muru sends itself, a static file or a file hint.

A changed page loses its `Content-Length` and a strong `ETag` becomes
weak, since its bytes are no longer the application's own. Compression
applies after the tag is inserted.

### Content security policy

The tag is loaded from the page's own origin, so a policy that allows
`'self'` for scripts needs nothing. When the policy gives scripts a
nonce, Muru puts the same nonce on the tag, and the toolbar uses it for
its stylesheet too. A policy that allows scripts only by hash, or only
from elsewhere, would refuse the tag, so the page gets none, and the run
says once, on the terminal, which path and why.

### The reserved prefix

The toolbar's files and data are under `/_muru/` on the application's
own address, answered by Muru without reaching the application:

| Path | Gives |
| --- | --- |
| `/_muru/toolbar.js`, `/_muru/toolbar.css` | the toolbar itself |
| `/_muru/requests/ID.json` | the record, as `muructl debug request ID --json` prints it |
| `/_muru/requests/ID/panels/NAME.html` | one panel of the record, drawn |

They answer GET only, with or without `--toolbar`, and only to requests
for `localhost`, a loopback address or a name the run was given with
`--host`, which stops another site reaching them through a DNS name it
points at your machine. A request a browser makes from another site's
page is refused. A route of the application under `/_muru/` is not
reached during a development run.

Behind a development proxy that serves the run under a path, as
jupyter-server-proxy and code-server do, the tag carries the path the
proxy sends in `X-Forwarded-Prefix`.

A page served by another server, such as a front end's own development
server calling a Python API that Muru serves, gets no toolbar, since
Muru never sees it; the API's requests still carry the timings below and
have records.

## Timings in the browser

Every response of a development run carries a `Server-Timing` header:

```
Server-Timing: queue;dur=0.21, app;dur=17.93, muru;desc="01JCZ8X4R3T5V6W7Y8Z9A0B1C2"
```

- `queue` is how long the request waited for a worker, in milliseconds.

- `app` is the time from handing the request to the worker until the
  response head arrived.

- `muru` carries the request's ID, the same as `X-Request-ID`.

The browser's developer tools show the figures with no setup: in the
Network tab, select a request and open its Timing section, where Chrome
and Firefox list them under Server Timing. A static file, a redirect or
an error answered by Muru itself carries only `muru`, since no worker
was involved.

Browsers show the `muru` entry differently: Safari shows the request's
ID alone, and Chrome shows `muru` and the ID cut short to fit its
column. The full ID is in the same response's `X-Request-ID` header, and
`Muru-Request-Link` is the address of its page.

There is no total, since the headers are sent before the body is
complete. The total and the time to the last byte are in the request's
record.

## A request's record

Each response also carries `Muru-Request-Link`, the address of the
request's page on the run's dashboard:

```
Muru-Request-Link: http://127.0.0.1:8081/request/01JCZ8X4R3T5V6W7Y8Z9A0B1C2
```

The address is the dashboard's own, or the one in `MURU_DASHBOARD_URL`
when the run's environment sets it, for a dashboard reached through a
tunnel or a proxy. With `--dashboard off` the header is not sent.

The page shows the record as panels:

- **Timing:** the request's phases from its arrival: the wait for a
  worker, the application until the response head, and sending the
  response, each with when it started and how long it took. Below them
  are the worker's own phases: until the application called
  `start_response` (or sent `http.response.start`), from its first body
  byte to its last, and until it ended, which for WSGI is `close()`
  returning.

- **Where:** the application and release, host and virtual host,
  mount, worker group, worker and the thread that ran the request,
  client and client class, the listener,
  internal redirects, a handoff between WSGI and ASGI, whether the
  request was sent again to another worker, and the routing snapshot.

- **Response:** the status, the bytes sent and received, whether the
  response was [compressed](compression.md) and why not when it was not
  (compression is off for the route, the application asked for none,
  the response was already encoded, its type is not one compressed, the
  body is under `min_size`, the client accepts none of the encodings
  offered), what the application produced before compression and where
  it came from (an iterable, `write()`, `wsgi.file_wrapper`, ASGI body
  messages or a file sent by Muru itself), and the request headers as
  they arrived and the response headers as they were sent.

- **Logs:** the log records and warnings the application made while
  serving the request, each with its time on the request's clock, its
  level, logger and message, and a traceback when the record has one.
  They are the records the `muru_request_id` tag names, which is every
  record made in the request's thread or task. The first 200 are kept
  and the rest counted. Output from `print()` is not kept, since it
  names no request.

- **Errors:** the exception the application did not catch, whether it
  was raised calling the application or while producing the body, and
  its traceback, as the development error page shows it; or, for an
  error Muru answered itself, its reason.

- **Connection:** the connection's ID, the request's position on it,
  how long the connection was idle before the request, for the first
  request on an HTTPS connection how long the handshake took, and
  whether Muru closed the connection after this response because it had
  served its share of requests.

The dashboard's recent requests link to each request's page, and
`/v1/requests/ID` on the dashboard's address gives the record as JSON.
A record still being written, for a response that is still streaming,
says so; reloading the page shows the rest once the response has ended.
The worker sends its part of the record just after the response, so a
record is complete a moment after the last byte arrives. If the worker
stopped before sending it, the Timing panel says so and shows the rest.

The [access log](logging.md)'s JSON lines name the connection too, as
`connection` and `connection_requests`, in every mode.

### What is kept

The run keeps the records of its last 1000 requests in memory, fewer
when they would take more than 32 MiB. They are kept across reloads and
are gone when the run stops. A request whose record has been dropped, or
one the run never served, is answered as no longer kept.

Bodies are never kept, only their sizes. Header values that may hold
secrets are shown as `***`: `Cookie`, `Set-Cookie`, `Authorization` and
`Proxy-Authorization` always, any header whose name contains SECRET,
TOKEN, PASSWORD, PASSWD, KEY, CREDENTIAL or AUTH or ends in `-DSN`, and
the password of a URL in any other header.

## From the command line

`muructl debug request` prints a record from the run's admin socket,
found from the project directory as the other `muructl debug` commands
find it:

```console
$ muructl debug request 01JCZ8X4R3T5V6W7Y8Z9A0B1C2
GET /orders/42 HTTP/1.1  500  2.0ms  at 10:42:07.123
request 01JCZ8X4R3T5V6W7Y8Z9A0B1C2

Timing
  request          2.0ms  from 0.0ms
    queue          0.0ms  from 0.0ms
    application    1.9ms  from 0.0ms
    send           0.0ms  from 1.9ms
    worker         1.8ms  from 0.0ms

Where
  application       shop@r3
  host              localhost
  mount             default
  group             default
  worker            0
  thread            muru-app-0
  client            127.0.0.1
  client class      default
  listener          127.0.0.1:8080
  sent again        no
  routing snapshot  0

Response
  status      500 app_error
  sent        351B
  received    0B
  compressed  no: the body is under min_size
  Request headers
    host: localhost:8080
    accept: text/html
    cookie: ***
  Response headers
    content-type: text/html; charset=utf-8
    ...

Logs
  Log records
    0.3ms  WARNING  shop.orders  order 42 has no lines
    0.5ms  ERROR  muru  the application raised an exception
      Traceback (most recent call last):
        ...
      KeyError: 'lines'

Errors
  outcome    failed
  reason     app_error
  raised in  call
  error      the application raised an exception
  traceback
    Traceback (most recent call last):
      ...
    KeyError: 'lines'

Connection
  id            01JCZ8X4R3T5V6W7Y8Z9A0B1C0
  position      1
  idle before   0.1ms
  closed after  no
```

`--json` prints the whole record, in the shape the
[JSON output reference](../reference/json-output.md) describes. The
ID is the one in `X-Request-ID`, in the `muru` entry of `Server-Timing`
and in the access log.

A coding agent reads the same record through the
[MCP server](coding-agents.md): its `debug` tool takes `request` with the
request's ID.
