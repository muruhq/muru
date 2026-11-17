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

The tutorial [debugging a page](../tutorials/debugging-a-page.md) uses
the toolbar and tracing to find and fix a page's faults, one panel at a
time.

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
and how many log records, errors and [hints](#hints) it had. Clicking the badge opens the
record's panels, the same ones the [dashboard's request
page](#a-requests-record) shows, as tabs, and "open in dashboard" opens
that page. A page whose response is still streaming updates as the
record fills in. The toolbar only shows: it has no forms and changes
nothing.

The application is not changed. Muru inserts one tag before the page's
`</body>`, or at the end of a page that has none, such as the error
pages Flask and other Werkzeug applications send, where the browser
puts it into the page's body all the same:

```html
<script src="/_muru/toolbar.js" data-request="01JCZ8X4R3T5V6W7Y8Z9A0B1C2" async></script>
```

It goes into a page a browser loads as a document, so `fetch()` and htmx
requests, which say otherwise in `Sec-Fetch-Dest`, are left as they are.
A client that sends no `Sec-Fetch-Dest`, such as curl, `requests` or the
application calling itself, gets the tag before a `</body>`, but a
response without one, such as a fragment of HTML, is left as it came:
the tag goes at the end only for a request that said
`Sec-Fetch-Dest: document`, as browsers do.
Muru never changes:

- a response to anything but GET, or one whose status is 204, 206 or
  304;
- a response that is not `text/html`, or has an empty body;
- a response the application encoded itself (`Content-Encoding`) or
  marked `Cache-Control: no-transform`;
- a file Muru sends itself, a static file or a file hint.

A changed page loses its `Content-Length` and a strong `ETag` becomes
weak, since its bytes are no longer the application's own. Compression
applies after the tag is inserted.

### Every request the page made

The toolbar's "This page" tab lists every request the page has made:
the document itself, its stylesheets, scripts and images, and the calls
its scripts made with `fetch()` or `XMLHttpRequest`, as a waterfall on
the page's clock. The list and its timings come from the browser, and
the toolbar matches each request to its record by the ID in its
`Server-Timing` header, so nothing in the page's own code is changed.
Requests the page makes later are added as they end.

Each row has:

- the request's address, and what made it, as the browser names it:
  `navigation` for the document, `script`, `css`, `img`, `fetch`,
  `xmlhttprequest` and so on;

- its status, and how long it took, from the browser starting it until
  the last byte of its response arrived;

- a bar placed where the request falls on the page's clock, with the
  time it waited for a worker drawn in blue where the request reached
  Muru, so a request queued behind another of the page's requests shows
  as one;

- from its record, how long it waited for a worker, the worker that
  served it (`edge` for a request Muru answered itself, such as a static
  file), and its log records and errors;

- its connection: a short label for each connection the page's requests
  used, `c1`, `c2` and so on in the order they first appear, with the
  request's position on it, and whether the browser opened the
  connection for it (`new`) or used one already open (`reused`);

- the protocol the browser used, such as `http/1.1` or `h2`.

Clicking a row opens that request's record: the tabs show its panels,
and "this page's request" goes back to the document's. A request whose
response is still streaming is refreshed until it ends.

Some rows have the browser's figures and no record:

- a response the browser took from its cache is marked `cache`, since
  no request reached Muru, rather than shown with the record of the
  response it was first cached from;

- a request to another origin, such as a font from a CDN;

- a request whose record is no longer kept, which says so.

The browser decides what the list holds, and the tab states its limits:

- A request is listed when its response ends, so a streamed response or
  a long-running fetch appears only once it has finished.

- WebSockets are not listed.

- The browser holds 250 of a page's requests by default until the
  toolbar starts, and drops the rest; requests made after the toolbar
  started are all listed. The tab says when the browser's list was full.

- Redirects that led to the page are not shown: the document's row is
  the response that ended them. The History tab shows them.

- A page that is not a secure context gets no `Server-Timing` from the
  browser, so only the document's own row has its record. `localhost`,
  a loopback address and an HTTPS page are secure contexts; a name given
  with `--host` and served over plain HTTP is not, and is served over
  HTTPS with [`--https`](https.md).

### The pages before this one

The toolbar's History tab lists the run's recent document requests,
newest first: the pages a browser loaded and the forms it submitted,
each with when it arrived, its method and target, status, total time,
time waiting for a worker, the worker, and its log records and errors.
The entry of the page the toolbar is on is marked "this page".

A redirect is shown with the request it led to, so a form's POST that
redirects to a page, or a chain of redirects, reads as one entry with
its steps in order: the POST that did the work, then the GET the
browser made of its `Location`. A request is taken as following a
redirect when it is the next document request from the same client, to
the redirect's `Location`, within 5 seconds of the redirect's response
ending.

Clicking a step opens its record, with its own panels, as clicking a row
of "This page" does, so the queries and log records of the POST are a
click away from the page it led to. The list comes from the records the
run keeps, its last 1000 requests, so a request whose record is no
longer kept is not listed, and at most the newest 100 entries are shown.

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
| `/_muru/requests?ids=ID,ID` | what the "This page" tab shows of each request named, at most 100 |
| `/_muru/history` | what the History tab shows: the recent document requests, redirects joined to where they led |

`requests?ids=` answers with `requests`, one entry for each request
named that is still kept, with its method, target, status, times,
worker, counts of log records and errors, connection and the address of
its page on the dashboard, and `not_kept`, the IDs of the rest.
`history` answers `entries`, newest first, each with its `steps` in
order, in the same shape.

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

- `import`, when the worker loaded modules for the first time while the
  request ran and before its response head, is how long that took, the
  modules each one imported in turn included. A module is loaded once in
  each worker, so the entry is on the first request that needed it.

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

The page shows the record as panels, the first of them **Hints**, what
stands out in the rest, which [has a section of its own](#hints):

- **Timing:** the request's phases from its arrival, each with when it
  started, how long it took and a bar placed where it falls in the
  request, and the moments that mark it, each in italics, with its time
  under Starts, a dash under Takes, a diamond on its row and a faint
  dashed line through every row, darker while the pointer is over the
  moment's row:

  - **waiting for a worker**, until one was free to take the request;

  - **in the worker**, the worker's own account, from receiving the
    request: **running the application**, from the end of the waits it
    started with until the application called `start_response` or sent
    `http.response.start`, the moment *response started*, **producing
    the body** from its first byte to its last, and **finishing**, until
    the body's `close()` returned or the ASGI application returned;

  - *response head at the edge*, the moment the response's status and
    headers came back from the worker;

  - **sending the response**, from then until the last byte was sent to
    the client.

  Under "in the worker", marked where they happened, are the waits that
  were not the request's own work:

  - **waiting for a thread**, from the worker taking a WSGI request
    until one of its threads started it;

  - **waiting for the GIL**, each time that thread, or the event loop
    for an ASGI request, waited to run Python for the request, counting
    the waits as an ASGI request's `receive()` calls were woken;

  - **event loop held**, each stretch during the request in which the
    worker's event loop did not get to run. It says the loop was
    blocked, by this request or another one on it, not what blocked it.

  Beside them is **importing modules**: each module the request loaded
  for the first time in this worker, from the import that asked for it
  to the end of its code, as a segment named by the module. A module it
  imports in turn is within the segment of the one that imported it,
  not drawn again. A module already loaded, by the host before the
  fork or by an earlier request in the worker, costs nothing and is not
  shown, so the row is on a worker's first request to a page and gone
  from the second. Loading such modules before the worker serves is
  what the [`preload_modules`](../explanation/preload.md#modules-loaded-on-first-use)
  setting does.

  A row of several waits, stretches or modules draws each of them as a
  segment on its one line, says how many under Starts and gives their
  total under Takes; a row of one draws it as an ordinary bar. The first
  64 waits for the GIL, and the 64 longest stretches with the loop held,
  are drawn; the rest are counted.

  Pointing at a bar, a segment or a moment, giving it keyboard focus
  with Tab, or tapping it, shows what it is: its name, when it started,
  how long it took, and for a segment which of the row's it is.

  A table under the waterfall, "During the request", gives the same
  with what cannot be placed on it: the GIL waits in all with how many
  there were, the stretches with the loop held in all with how many
  there were and the longest, and the garbage collections that ran
  while the request was in the worker, how many and how long they took,
  by generation. A collection pauses every thread of the worker, so it
  holds up whatever requests are in flight. On a free-threaded build
  there is no GIL, and no GIL wait is shown.

  The table's "modules loaded on first use" row gives how many modules
  the request loaded and how long they took, and "Modules loaded on
  first use" below it opens to list each of them in the order they
  began: when it started, how long it took and the file and line whose
  import asked for it, a module imported by another indented beneath it.
  A record lists the first 200 and counts the rest. With
  [`lazy_imports`](lazy-imports.md) on, a module whose deferred import the
  request used is marked "(deferred)", and its file and line are those of
  the code that used it.

  The panel lists the same under "What the phases are".

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
  error Muru answered itself, its reason. When the application caught
  the exception itself and answered 500, as Flask and Django do, the
  panel says the application answered it and shows the first error the
  request logged with a traceback. A request that ran past a
  timeout, or whose worker was killed, has the evidence Muru captured,
  as [below](#evidence-of-a-timeout-or-a-killed-worker).

- **Connection:** the connection's ID, the request's position on it,
  how long the connection was idle before the request, for the first
  request on an HTTPS connection how long the handshake took, and
  whether Muru closed the connection after this response because it had
  served its share of requests.

Panels from the worker follow these, drawn the same way in the toolbar,
on the dashboard and by `muructl debug request`: values, tables, spans
on the request's clock, and text. Each such panel keeps a limited number
of entries for a request, and says how many were past the limit and not
kept. The application can add its own, as [adding
panels](panels.md) shows. With [tracing](tracing.md#in-the-toolbar)
on, Muru adds **Trace**, the request's tree of calls, queries and template
renders, with the operations that failed marked; **SQL**, its queries grouped by statement, which shows an N+1
pattern as one statement run many times; **Outbound**, its calls to
other services grouped by endpoint, which shows the same pattern for an
HTTP or gRPC call made once per row; and **Exceptions**, the
exceptions that escaped or were handled. Each request the page view
lists opens to its own panels.

The dashboard's recent requests link to each request's page, and
`/v1/requests/ID` on the dashboard's address gives the record as JSON.
A record still being written, for a response that is still streaming,
says so; reloading the page shows the rest once the response has ended.
The worker sends its part of the record just after the response, so a
record is complete a moment after the last byte arrives. If the worker
stopped before sending it, or the edge stopped waiting for it at the
request's deadline, the Timing, Logs and Errors panels say so in place
of what the worker would have given, and show the rest.

### Hints

The Hints panel lists what stands out in the request's record, each
hint saying where it comes from, the panel and the figure, so it is one
place to look before the others. The badge counts the hints, and the
toolbar opens on this panel when there are any. Hints read the record
and change nothing. Each has a threshold, at or past which it is shown:

| Hint | From | Shown when |
| --- | --- | --- |
| a statement run many times in one request, the N+1 pattern | SQL | the same statement, after its values are made placeholders, ran 5 times or more |
| an endpoint called many times in one request | Outbound | the same method, host and path was called 5 times or more |
| a slow query or call | SQL, Outbound | one query or call took 25% or more of the request's total time, and at least 10 ms |
| an HTML or JSON response that went out uncompressed | Response | it was 1 KiB or more, and was not compressed because compression is off for the route, the application asked for none, the response says `no-transform`, its type is not one the route compresses, the client accepts none of the encodings offered, or the encoder could not start; the hint gives the reason |
| the event loop held | Timing, Trace | the worker's event loop was held for 100 ms or more during the request; the hint names the operation the Trace panel shows running in the middle of that stretch, when tracing recorded one |
| a long wait for a worker | Timing | the request waited 100 ms or more for a worker, which says the run had too few for the page's requests; the hint names `processes` (`--processes`) |
| a long wait for a thread | Timing | a WSGI request waited 100 ms or more for a thread of its worker; the hint names `threads` (`--threads`) |
| garbage collection | Timing | collections during the request took 50 ms or more in all, or there were 20 or more of them |
| modules loaded on first use | Timing | the modules the worker loaded for the first time during the request took 25% or more of its total time, and at least 50 ms; the hint names the longest and `preload_modules`, which loads them before the worker serves |
| an answer of Muru's own | Errors | Muru answered the request itself for a timeout or a size limit, such as 504 `deadline` or 413 `body_too_large`; the hint names the setting, such as `request_deadline` (`--timeout deadline=` for a run) |

At most 3 hints of one kind are shown, the strongest first. The same
hints are the first panel of the record's JSON, `hints`, and of
`muructl debug request`, so a coding agent reading the record gets them
too.

### Evidence of a timeout or a killed worker

When a request fails because it ran too long, Muru captures where it
was, and the request's record keeps it in the Errors panel, one table
for each capture, named by the event and its time:

- `request.soft_timeout`: the request ran past the [soft
  timeout](timeouts.md#the-soft-timeout). The stack of its thread is
  taken just before the timeout interrupts it.

- `request.past_deadline`: the request was still running when the edge
  answered it 504 at its [deadline](timeouts.md#past-the-deadline). The
  stack is its thread's a second or two after the deadline. If the
  request later ends while its record is kept, a `request.ended` entry
  says so, with how long it had been running when it was last seen.

- `worker.timeout_killed`: the request's worker was replaced for the
  [hard timeout](timeouts.md#the-hard-timeout), a stalled event loop
  or GIL, or reports that stopped.

- `worker.restarted`: the worker was [restarted with
  `--force`](recycling.md#restarting-a-worker-by-hand) while it served
  the request.

Each has the event's message, as `muructl events` shows it, and the
request's own stack, outermost frame first and its innermost 64 frames
kept. For a killed worker it also names the worker, its pid and why it
was killed, gives the stacks of the worker's other threads that were
serving requests, each with its request, and links to the record of
each other request in flight on that worker. A capture goes to the
record of every request it names that is still kept, at most 8 for one
record. A worker that cannot answer for its stacks, such as one whose
thread holds the GIL in C, gives none: a request past its deadline has
the capture without a stack, and a killed worker's capture reaches the
records of the requests it gave up and of those the edge still had in
flight on it, with the event's message, the worker, its pid and why,
but no stacks. Its stacks go to the log through `faulthandler`.

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

Query parameters that may hold secrets are shown as `***` too, in the
request's target, its internal redirects and the `Referer`, `Location`
and `Content-Location` headers: `/books?token=abc&page=2` is kept as
`/books?token=***&page=2`. A parameter's name is compared with its case,
`-`, `_` and `.` ignored, and for a name like `user[password]`, its last
bracketed part, so `access_token`, `accessToken` and `Access-Token` are
one name. These names are secrets: `password`, `passwd`, `pwd`,
`secret`, `token`, `access_token`, `refresh_token`, `id_token`,
`api_key`, `key`, `client_secret`, `auth`, `jwt`, `code`, `oauth_token`,
`private_token`, `session`, `session_id`, `sessid`, `jsessionid`,
`phpsessid`, `sig`, `signature`, `X-Amz-Signature`,
`X-Amz-Security-Token` and `X-Goog-Signature`, and any name ending in
`token`, `secret`, `password` or `passwd`, such as `csrf_token`. The
access log writes the query as it came unless
[`access_log_query`](logging.md#the-query-string) masks or drops it.

## From the command line

`muructl debug request` prints a record from the run's admin socket,
found from the project directory as the other `muructl debug` commands
find it:

```console
$ muructl debug request 01JCZ8X4R3T5V6W7Y8Z9A0B1C2
GET /orders/42 HTTP/1.1  500  2.0ms  at 10:42:07.123
request 01JCZ8X4R3T5V6W7Y8Z9A0B1C2

Timing
  request                      2.0ms  from 0.0ms
    waiting for a worker       0.0ms  from 0.0ms
    in the worker              1.8ms  from 0.0ms
      running the application  1.7ms  from 0.1ms
      response started         at 1.8ms
      producing the body       0.0ms  from 1.8ms
      finishing                0.0ms  from 1.8ms
    response head at the edge  at 1.9ms
    sending the response       0.0ms  from 1.9ms

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
