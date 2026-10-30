# distill
### Wraps the `distill-lopdf` CLI to execute HTTP requests via `4D.SystemWorker`.

> distill.new (class : 4D.Class)

| Parameter | Type | | Description |
| --- | --- | --- | --- |
| class | 4D.Class | -> | Optional custom controller class; must extend `_distill_Controller` (default: `cs.lopdf._distill_Controller`) |

## Description

`cs.lopdf.distill` extends [`_CLI`](_CLI.md) and wraps the system `distill-lopdf` executable. It provides `version()` to inspect the installed build and `execute()` to run one or more commands, with support for both synchronous and asynchronous operation.

If a class that does not extend `_distill_Controller` is passed, the constructor silently falls back to `cs.lopdf._distill_Controller`. Detection walks the full superclass chain.

### Properties

| Property | Type | Description |
| --- | --- | --- |
| onData | 4D.Function | Called by the controller on each parsed progress update (async mode only) |
| onResponse | 4D.Function | Called by the controller when a command completes |
| onError | 4D.Function | Called by the controller on worker error |
| onTerminate | 4D.Function | Called by the controller when the worker terminates |
| worker | 4D.SystemWorker | The currently active worker (read-only, from controller) |
| controller | cs.lopdf._distill_Controller | The attached controller instance (read-only) |

### Methods

#### execute (option : Variant; events : Object) → Collection

Runs one or more commands built from an options collection, in sync or async mode.

| Parameter | Type | | Description |
| --- | --- | --- | --- |
| option | Object \| Collection | -> | A single options collection or a collection of options collections (see below) |
| events | Object | -> | Event callbacks; if `events.onResponse` is set the call is asynchronous |
| Result | Collection | <- | Collection of stdout strings (one per command), or `Null` in async mode |

**Sync mode** (no `events.onResponse`): each worker runs to completion before the next starts. `controller.stdOut` is collected into `$results` and `controller.clear()` is called after each command. The method returns a Collection of raw stdout strings, or `Null` entries where `-o` / `--output` was specified.

**Async mode** (`events.onResponse` present): workers are started without waiting. The method returns `Null` immediately. Progress updates are delivered to `events.onData` as percentage objects (see [`_distill_Controller`](_distill_Controller.md)). 

**option element types** — each `option` is a flat collection where elements are interpreted by type:

| Element type | Behaviour |
| --- | --- |
| Text | Shell-escaped and appended as-is, except for the special values below |
| Real / Integer | Appended as a numeric string |
| Boolean / Null | Ignored |
| 4D.File / 4D.Folder | Platform path is shell-escaped and appended |
| Object with `.data` | Payload posted to worker stdin |
| Object with `.file` | Blob or Text posted to worker stdin (streaming mode) |

**Special text values:**

| Value | Effect |
| --- | --- |
| `"-#"` | Suppressed (added automatically in async mode) |
| `"-o"` / `"--output"` | Marks output as going to a file; `Null` is pushed to results instead of stdout |
| `"@-"` / `"-"` | Activates streaming mode; stdin payload is posted from `.file` |

## See also

- [`_CLI`](_CLI.md) — parent class providing executable resolution and shell escaping
- [`_distill_Controller`](_distill_Controller.md) — default controller; handles progress parsing and event forwarding
- [`_CLI_Controller`](_CLI_Controller.md) — base controller providing the command queue
