# _distill_Controller
### Extends `_CLI_Controller` with stdout/stderr accumulation.

> _distill_Controller.new (CLI : cs.lopdf._CLI)

| Parameter | Type | | Description |
| --- | --- | --- | --- |
| CLI | cs.lopdf._CLI | -> | The owning `_CLI` instance |

## Description

`_distill_Controller` is the default controller used by [`distill`](distill.md). It inherits all command-queue and worker-management behaviour from [`_CLI_Controller`](_CLI_Controller.md) and overrides the data and response event handlers to:

- Accumulate stdout into `stdOut` for synchronous callers.
- Forward `onResponse`, `onError`, and `onTerminate` events to the corresponding callbacks on the owning `distill` instance.

### Properties

In addition to properties inherited from `_CLI_Controller`:

| Property | Type | Description |
| --- | --- | --- |
| stdOut | Text | Accumulated stdout text from the last command |
| stdErr | Text | Accumulated stderr text (consumed incrementally during progress parsing) |

### Methods

#### clear () → cs.lopdf._distill_Controller

Resets `stdOut` and `stdErr` to empty strings. Called automatically by the constructor and by `distill.execute` between successive synchronous commands.

| Result | Type | Description |
| --- | --- | --- |
| Result | cs.lopdf._distill_Controller | `This` — enables chaining |

### Overridden event callbacks

#### onData ($worker : 4D.SystemWorker; $params : Object)

Appends `$params.data` to `stdOut`.

#### onDataError ($worker : 4D.SystemWorker; $params : Object)

Handles distill-lopdf's stderr stream. When `distill.onData` is set, appends incoming data to `stdErr`. For each match found, calls `distill.onData` with a context object:

| Property | Type | Description |
| --- | --- | --- |
| percentage | Real | Parsed progress percentage (0–100) |
| context | Object | Per-command context from `SYSTEM_WORKER_CONTEXT` keyed by worker PID |

Consumed characters are trimmed from `stdErr` after each scan pass, so partial lines are held until a complete match arrives.

When `distill.onData` is not set, stderr is silently discarded.

#### onResponse ($worker : 4D.SystemWorker; $params : Object)

Forwards to `distill.onResponse` if set.

#### onError ($worker : 4D.SystemWorker; $params : Object)

Forwards to `distill.onError` if set.

#### onTerminate ($worker : 4D.SystemWorker; $params : Object)

Forwards to `distill.onTerminate` if set.

## See also

- [`_CLI_Controller`](_CLI_Controller.md) — parent class
- [`distill`](distill.md) — sets `onData`, `onResponse`, `onError`, `onTerminate` which this controller forwards to
