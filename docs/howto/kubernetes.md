# A Kubernetes Deployment

An image built as [in a container](containers.md#an-image) runs in a pod
as it runs under Docker. Its settings come from the environment, so the
whole configuration is in the Deployment.

## The Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shop
spec:
  replicas: 2
  selector:
    matchLabels: {app: shop}
  template:
    metadata:
      labels: {app: shop}
    spec:
      terminationGracePeriodSeconds: 30
      securityContext:
        runAsNonRoot: true
        seccompProfile: {type: RuntimeDefault}
      containers:
        - name: shop
          image: registry.example.com/shop:1.4.2
          env:
            - {name: MURU_SERVE_SHUTDOWN_DELAY, value: "5s"}
            - {name: MURU_SERVE_SHUTDOWN_BUDGET, value: "25s"}
            - {name: MURU_SERVE_TRUSTED_PROXY, value: "private"}
            - {name: MURU_SERVE_LOG_FORMAT, value: "json"}
            - {name: MURU_SERVE_PROCESSES, value: "2"}
            - name: POD_NAME
              valueFrom: {fieldRef: {fieldPath: metadata.name}}
            - name: POD_NAMESPACE
              valueFrom: {fieldRef: {fieldPath: metadata.namespace}}
            - name: NODE_NAME
              valueFrom: {fieldRef: {fieldPath: spec.nodeName}}
          ports:
            - {name: http, containerPort: 8080}
            - {name: admin, containerPort: 9090}
          startupProbe:
            httpGet: {path: /startupz, port: admin}
            periodSeconds: 2
            failureThreshold: 60
          livenessProbe:
            httpGet: {path: /livez, port: admin}
          readinessProbe:
            httpGet: {path: /readyz, port: admin}
            periodSeconds: 2
            failureThreshold: 1
          resources:
            requests: {cpu: "1", memory: 512Mi}
            limits: {cpu: "2", memory: 1Gi}
          resizePolicy:
            - {resourceName: cpu, restartPolicy: NotRequired}
            - {resourceName: memory, restartPolicy: NotRequired}
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities: {drop: ["ALL"]}
          volumeMounts:
            - {name: run, mountPath: /run/muru}
            - {name: tmp, mountPath: /tmp}
      volumes:
        - name: run
          emptyDir: {sizeLimit: 2Gi}
        - name: tmp
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: shop
spec:
  selector: {app: shop}
  ports:
    - {name: http, port: 80, targetPort: http}
```

The pieces, in turn:

- **Two ports.** The application is on `http` and the probes and metrics
  on `admin`. The Service exposes only `http`, so an Ingress in front of
  it never reaches the admin port. A NetworkPolicy can limit the admin
  port to the kubelet and Prometheus.
- **Three probes.** `/startupz` holds the other two off until the
  application has loaded, however long its import takes. `/livez`
  restarts a container whose edge or host has stopped answering, never
  one whose workers are only busy. `/readyz` takes a pod out of the
  Service without restarting it, while no workers are ready or once it is
  stopping. [Probes and metrics](containers.md#probes-and-metrics)
  describes each.
- **Stopping.** SIGTERM fails `/readyz` at once. For
  `MURU_SERVE_SHUTDOWN_DELAY` the pod keeps serving while the Service's
  endpoints and the ingress stop sending to it, closing each connection
  after its response. Then the requests in flight finish and the
  container exits, all within `MURU_SERVE_SHUTDOWN_BUDGET`, which is set
  under `terminationGracePeriodSeconds` and has to cover the delay, the
  requests, the workers' `exit_grace` (2 seconds by default) and a second
  for the edge. A rolling update loses no
  requests and needs no `preStop` hook; [stopping](containers.md#stopping)
  covers the sequence.
- **Restarts in place.** A container restarted in place, by a
  failed liveness probe or anything else, keeps its pod, so its endpoint
  is not marked terminating and leaves the Service only once the
  readiness probe has failed `failureThreshold` times. Keep
  `MURU_SERVE_SHUTDOWN_DELAY` longer than `periodSeconds` times
  `failureThreshold` of the readiness probe, here 5 seconds against 2.
  Muru answers `/readyz` itself rather than through a worker, so a
  `failureThreshold` of 1 does not take a busy pod out of the Service.
- **The proxy in front.** The ingress controller's connections come from
  pod addresses, so `MURU_SERVE_TRUSTED_PROXY=private` applies its
  `Forwarded` and `X-Forwarded-*` headers. Name its range instead to
  trust nothing else; [behind a proxy](behind-a-proxy.md) has the
  details.
- **The security context.** The pod passes the `restricted` Pod Security
  Standard: a user ID other than root, any ID the platform assigns, no
  privilege escalation, no capabilities, and a read-only root filesystem.
  Only `/run/muru` and `/tmp` are written, so they are the only volumes.
  Request bodies too large to hold in memory are spooled to `/run/muru`,
  up to `--spool-quota` (1 GB by default), so its `sizeLimit` is above
  that. A volume with `medium: Memory` would count those bodies against
  the container's memory limit. `runAsNonRoot` needs the image's `USER`
  to be a number, as in the [image](containers.md#an-image).
- **Resources.** `MURU_SERVE_PROCESSES` sets two workers, one for each
  CPU of the limit; nothing is worked out from the limits. With the
  memory limit, the memory guard replaces the largest worker when the
  container runs short, before the kernel kills one, and the start lines
  warn when two workers that each come to copy what the host holds would
  not fit. [Container limits](containers.md#container-limits) shows where
  the memory goes and the lines that report it. The `resizePolicy` lets
  the limits change without a restart; [resizing](#resizing) says what
  that changes.
- **Logs.** With `MURU_SERVE_LOG_FORMAT=json` every line on standard
  output is one JSON record, a traceback included, carrying the pod, its
  namespace and its node from the Downward API variables, which is what a
  collector such as Fluent Bit or Vector reads from the node.
  [Logs](containers.md#logs) shows the records.

## Resizing

Kubernetes can change a running container's CPU and memory in place,
through `kubectl patch --subresource resize` or a VerticalPodAutoscaler
in its `InPlaceOrRecreate` mode. With `restartPolicy: NotRequired` for
both, the container keeps running, and `muructl serve` reads its limits
when it uses them, so a resize applies at the next memory check:

- **More memory** gives the workers more room at once, and the memory
  guard works from the new limit.
- **Less memory** is safe while the workers fit under it. Kubernetes
  holds a decrease below what the container uses until its use drops.
- **More or fewer CPUs** change the time the same workers get.

muructl logs each change, with the old and the new value, and gives its
start warnings again against the new limits, so a decrease that leaves
the workers too little room says so. `muructl status` in the pod shows
the limits as they are now.

The worker count and a worker's `hard_limit` are configuration, not
limits, so a resize does not change them. Changing either is a change to
the Deployment, which rolls out new pods.

A VerticalPodAutoscaler in its `Recreate` mode evicts the pod and
creates another with the new resources. A HorizontalPodAutoscaler on
`muru_capacity_utilisation`, below, and a VerticalPodAutoscaler on CPU
and memory can run together, since they act on different figures.

## Autoscaling

A HorizontalPodAutoscaler scales on `muru_capacity_utilisation`, the
share of the worker slots in use averaged over 30 seconds, rather than
on CPU, which stays low while an application waits on its database with
every slot busy. Prometheus scrapes the admin port:

```yaml
template:
  metadata:
    annotations:
      prometheus.io/scrape: "true"
      prometheus.io/port: "9090"
```

The Prometheus adapter makes the metric a pods metric:

```yaml
rules:
  - seriesQuery: 'muru_capacity_utilisation{namespace!="",pod!=""}'
    resources:
      overrides:
        namespace: {resource: namespace}
        pod: {resource: pod}
    metricsQuery: 'avg(<<.Series>>{<<.LabelMatchers>>}) by (<<.GroupBy>>)'
```

and the autoscaler keeps it near 70 percent:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: shop
spec:
  scaleTargetRef: {apiVersion: apps/v1, kind: Deployment, name: shop}
  minReplicas: 2
  maxReplicas: 10
  metrics:
    - type: Pods
      pods:
        metric: {name: muru_capacity_utilisation}
        target: {type: AverageValue, averageValue: 700m}
```

KEDA can scale on the same figure, or on how long requests wait for a
slot, with a Prometheus trigger:

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: shop
spec:
  scaleTargetRef: {name: shop}
  minReplicaCount: 2
  maxReplicaCount: 10
  triggers:
    - type: prometheus
      metadata:
        serverAddress: http://prometheus.monitoring:9090
        query: >-
          histogram_quantile(0.95, sum by (le)
          (rate(muru_request_duration_seconds_bucket{app="shop",phase="queue"}[2m])))
        threshold: "0.1"
```

[Saturation for an autoscaler](metrics.md#saturation-for-an-autoscaler)
describes both figures.

## Migrations

Declare the migration as a [command](commands.md) of the project:

```toml
[tool.muru.commands.migrate]
description = "Apply database migrations"
run = ["python", "manage.py", "migrate", "--noinput"]
timeout = "30m"
```

A Job runs it from the new image, before the Deployment is updated to
it:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: shop-migrate-1-4-2
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      securityContext:
        runAsNonRoot: true
        seccompProfile: {type: RuntimeDefault}
      containers:
        - name: migrate
          image: registry.example.com/shop:1.4.2
          command: ["muructl", "command", "run", "migrate"]
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: {drop: ["ALL"]}
```

With no application named and nothing serving in the Job's pod,
`muructl` runs the command itself, with the image's interpreter, the
project's `[tool.muru.env]` variables and the command's timeout, and
exits with its status, so a failed migration fails the Job. Give the Job
the environment variables and secrets the Deployment's containers have,
since the migration reads the same settings. `parallelism` is 1 by
default, which keeps the migration to one run. An init container or a
Helm pre-upgrade hook runs the same command.

`muructl exec -- alembic upgrade head` runs a command that is not
declared, the same way.

## Looking inside a pod

`muructl` in the container reaches `muructl serve` through its run
directory, so `kubectl exec` runs the commands that read or adjust a
running application:

```
kubectl exec deploy/shop -- muructl status
kubectl exec deploy/shop -- muructl logs --access --follow
kubectl exec deploy/shop -- muructl debug stacks
kubectl exec deploy/shop -- muructl restart --all
kubectl exec deploy/shop -- muructl command run backup
```

Whoever may exec into the pod may use them, which RBAC decides.

The debugging commands work in the pod above as it is, with every
capability dropped, no privilege escalation and the `RuntimeDefault`
seccomp profile, and need no `SYS_PTRACE` or other change to its
security context. `muructl debug stacks` and `muructl debug profile`
reach the host, which reads its own workers: on Python 3.15 and later it
reads them from outside, as their parent and the same user, which the
kernel allows a process to do to its descendants; on earlier versions
it asks each worker, which needs nothing at all. In a namespace that
enforces the `restricted` Pod Security Standard nothing has to be
added.
