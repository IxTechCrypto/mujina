# REST API

Mujina exposes an HTTP API on port 7785 (ASCII "MU") for
monitoring and control. It binds to localhost by default. Set
`MUJINA_API_LISTEN` to override the listen address:

```bash
MUJINA_API_LISTEN="0.0.0.0" cargo run
```

The port defaults to 7785 if not specified, or you can
override it with `MUJINA_API_LISTEN="0.0.0.0:9000"`.

The API currently has no authentication or encryption, so
binding to a non-localhost address exposes it to the network
without access control. These will be addressed later.

An OpenAPI spec is served at `/api/v0/openapi.json`. A Swagger
UI is available at `/swagger-ui` for interactive browsing.

## Versioning

All endpoints live under `/api/v0/`. The v0 prefix signals an
unstable API: breaking changes are expected until Mujina
matures. Once the API stabilizes it will move to v1, and
backwards-incompatible changes will increment the version
number from there.

## Data model

The API models the miner as a single state tree. `GET /miner`
returns the complete snapshot; endpoints like `/boards` and
`/boards/{name}` return subtrees of the same structure.
Writable fields are typically updated with PATCH, which applies
a partial update to the relevant part of the tree.

## Conventions

### Null values

Sensor readings (`rpm`, `temperature_c`, `voltage_v`, etc.) are
nullable. A null value means the hardware read failed or the
sensor is not present. Clients should treat null as "unknown,"
not zero.

Target fields (`target_percent`) are also nullable. Null means
no override has been set through the API; the miner is managing
the value on its own (e.g. a startup default or a control
algorithm). Once a client sets a value, the field reflects that
value until changed again.

### Units

All values are in raw SI-ish units. Clients are responsible for
formatting and unit conversion.

| Field suffix | Unit                   |
|--------------|------------------------|
| `_secs`      | seconds                |
| `_c`         | degrees Celsius        |
| `_v`         | volts                  |
| `_a`         | amperes                |
| `_w`         | watts                  |
| `rpm`        | revolutions per minute |
| `hashrate`   | hashes per second      |

Percentage fields (`percent`, `target_percent`) are integers
0--100.

### Naming

Boards and sources are identified by a URL-friendly `name` field
(e.g. `bitaxe-e2f56f9b`). These names appear in URL paths for
single-resource endpoints like `/api/v0/boards/{name}`.

## Endpoints

The OpenAPI spec is the authoritative endpoint reference. This
table is a summary and may not be exhaustive.

### Miner (aggregate)

| Method | Path         | Description                    |
|--------|--------------|--------------------------------|
| GET    | `/miner`     | Full state snapshot            |
| PATCH  | `/miner`     | Update miner config (e.g. pause) |
| GET    | `/settings`  | Miner name and pool settings   |
| PATCH  | `/settings`  | Save miner name and/or pool    |

`/settings` is persisted configuration, not live state. The daemon reads
its name and pool once at startup, so a PATCH is saved immediately but
takes effect on the next restart; the response's `restart_required` says
whether the saved values differ from the running ones. The pool password
is never returned — `pool.password_set` reports only whether one is
stored, and omitting `password` from a PATCH keeps the stored value.

The miner's name is also sent to the pool as the worker suffix
(`user.name`), so `worker_username` in the response shows the full string
that will be authorized.

### Boards

| Method | Path                       | Description                |
|--------|----------------------------|----------------------------|
| GET    | `/boards`                  | List connected boards      |
| GET    | `/boards/{name}`           | Single board detail        |
| PATCH  | `/boards/{name}/fan`       | Fan control policy         |
| PATCH  | `/boards/{name}/tuning`    | Manual frequency/voltage   |
| GET    | `/boards/{name}/autotune`  | Auto-tuning status         |
| PATCH  | `/boards/{name}/autotune`  | Enable/configure auto-tune |

Fan control, manual tuning and auto-tuning are all per board: each board
runs its own independent auto-tuner, tuned against that board's own
measured hashrate.

### Sources

| Method | Path              | Description          |
|--------|-------------------|----------------------|
| GET    | `/sources`        | List job sources     |
| GET    | `/sources/{name}` | Single source detail |

### Health

| Method | Path      | Description          |
|--------|-----------|----------------------|
| GET    | `/health` | Returns "OK"         |

All paths are relative to `/api/v0`.

## Types

The request and response types are defined in Rust in the
`api_client::types` module
(`mujina-miner/src/api_client/types.rs`). These types are the
shared contract between the server and its clients (CLI, TUI).
The OpenAPI schema is derived from them automatically.
