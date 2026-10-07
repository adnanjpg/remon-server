# Screens

A screen is a page described as data. The client draws it with its renderer,
and the data comes from `POST /query`. The assistant composes screens today.
The command bar and the dashboard will use the same schema. Whoever writes a
spec picks _what_ to show. The numbers never pass through the writer, which
matters most when the writer is a model.

This is the contract between remon-server and remon-web and lives in both
repos. Change it in both, in the same step. We are pre-1.0: the schema is
replaced, not versioned.

## Spec

```json
{
	"title": "nginx vs postgres",
	"range": "24h",
	"root": {
		"type": "grid",
		"columns": 2,
		"children": [
			{
				"type": "line",
				"title": "CPU",
				"series": [
					{
						"label": "nginx",
						"query": {
							"namespace": "process",
							"field": "cpu_percent",
							"labels": { "name": "nginx" }
						}
					},
					{
						"label": "postgres",
						"query": {
							"namespace": "process",
							"field": "cpu_percent",
							"labels": { "name": "postgres" }
						}
					}
				]
			},
			{
				"type": "stat",
				"title": "Memory now",
				"query": { "namespace": "memory", "field": "used_percent" }
			},
			{ "type": "widget", "config": { "kind": "alert-timeline" } }
		]
	}
}
```

| field   | notes                                                                  |
| ------- | ---------------------------------------------------------------------- |
| `title` | required; trimmed, cut to 80 chars                                     |
| `range` | `30m` \| `1h` \| `6h` \| `24h` \| `7d` \| `30d`; default `1h`         |
| `root`  | a node                                                                 |

### Nodes

Every node has a `type`. Unknown types and unknown fields are rejected.

| type     | fields                                               | draws                                                                                    |
| -------- | ---------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `grid`   | `columns` 1-4 (default 1), `children` 1-12 nodes     | children left to right, wrapping after `columns`                                         |
| `tabs`   | `tabs`: 1-6 × `{ title, child }`                     | one child at a time                                                                      |
| `line`   | `title`, `series`: 1-8 × `{ label?, query }`, `range?` | history on shared axes; `range` overrides the screen's                                 |
| `stat`   | `title`, `query`                                     | one current value                                                                        |
| `table`  | `title`, `columns`: 1-6 × `{ label, query, agg? }`, `limit?` 1-50, `range?` | one row per label set, one column per query, ranked by the first column |
| `widget` | `title?`, `config`                                   | a built-in card. `config` is a remon-web `WidgetConfig`, e.g. `{ "kind": "cpu-detail" }` |

Limits per screen: at most 4 levels of nesting, 24 panels, and 16 queries.
A panel is a `line`, `stat`, `table` or `widget`. A `line` series and a
`table` column each count as one query.

### Query

```json
{ "namespace": "process", "field": "cpu_percent", "labels": { "name": "nginx" }, "limit": 5 }
```

These are the same metric names that alert rules and the assistant's
`query_metric` use.

- `line` series read history and need a charted namespace:

  | namespace       | label            | fields                                                                                                                                                                               |
  | --------------- | ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
  | `cpu`           | none             | `usage_percent`, `load_1m`, `load_5m`, `load_15m`, `steal_percent`, `iowait_percent`, `guest_percent`, `user_percent`, `system_percent`, `context_switches_per_sec`, `process_forks_per_sec` |
  | `memory`        | none             | `total_bytes`, `used_bytes`, `available_bytes`, `cached_bytes`, `swap_used_bytes`, `page_faults_minor_per_sec`, `page_faults_major_per_sec`, `swap_in_pages_per_sec`, `swap_out_pages_per_sec`, `used_percent` |
  | `disk`          | `mount_point`    | `total_bytes`, `used_bytes`, `available_bytes`, `read_bytes_per_sec`, `write_bytes_per_sec`, `inode_used_percent`, `read_iops`, `write_iops`, `io_util_percent`, `used_percent` |
  | `network`       | `interface_name` | `rx_bytes_per_sec`, `tx_bytes_per_sec`, `rx_packets_per_sec`, `tx_packets_per_sec`, `errors_in_per_sec`, `errors_out_per_sec`                                                        |
  | `network_total` | none             | same as `network`                                                                                                                                                                    |
  | `process`       | `name`           | `cpu_percent`, `memory_bytes`, `pid_count`, `disk_read_bps`, `disk_write_bps`                                                                                                         |
  | `docker`        | `container_id`   | `cpu_percent`, `memory_used_bytes`, `memory_limit_bytes`, `memory_percent`, `network_rx_bytes`, `network_tx_bytes`, `block_read_bytes`, `block_write_bytes`, `pids`                   |
  | `pressure`      | `resource`       | `some_avg10`, `some_avg60`, `some_avg300`, `full_avg10`, `full_avg60`, `full_avg300`                                                                                                 |

  `series_catalog()` in `src/storage/repositories/chart.rs` is the source of
  this table. A keyed namespace takes only its own label. If the label is left
  out, the series shows the `limit` busiest keys (default 10, at most 50).
  "Busiest" means the highest average over the window, where any tick in
  which a key was absent counts as zero (see `stats.avg` below).

- `stat` reads the current value through the alert resolver. It can use any
  namespace a rule can watch, including `probe`, `service`, `heartbeat`,
  `smart` and `components`. A `stat` query must match exactly one series.

- A `table` column has an optional `agg`. The default, `current`, reads the
  current value in the same way a `stat` does. `avg`, `max` and `min` read
  over the window: the table's own `range` if it has one, otherwise the
  screen's. These need a charted namespace, the same as a `line` series.
  Put `limit` on the `table`, not on its queries.

### Validation

`screen::validate` (`src/screen/mod.rs`) checks every spec and reports every
problem it finds, one per line, each prefixed with its path, e.g.
`root.children[1].series[0]: ...`. It rejects:

- a bad shape: unknown type or field, empty title, bad range, or a broken limit.
- a query that would draw an empty panel. The error says what does exist,
  for example `no process with name='apache' in the last 1h; seen: nginx,
  postgres`.
- a `stat` that matches more than one series. The error lists the label sets
  to choose from.
- a bad `widget` config. These are checked by the dashboard's rules, and for
  `probe-metric` the probe, metric and labels must exist.

On success, `validate` returns the spec in canonical form: defaults filled in,
titles trimmed, empty `labels` dropped, and widget configs normalized. A
`probe-metric` widget is given `unit` and `labelKey`.

## `POST /query`

The renderer sends all of a screen's queries (those sharing a window) in one
request.

```json
{
	"range": "24h",
	"max_points": 300,
	"queries": [
		{ "id": "a", "namespace": "process", "field": "cpu_percent", "labels": { "name": "nginx" } },
		{ "id": "b", "namespace": "memory", "field": "used_percent", "mode": "latest" }
	]
}
```

- Window: `range`, or `start`/`end` in unix seconds, but not both. The default
  is the last hour.
- `mode`:
  - `series` (default) reads history over the window.
  - `latest` reads the current value.
  - `summary` returns no points. Each series carries
    `stats: { avg, min, max }` over the window instead. A table column whose
    `agg` is not `current` uses this mode.
- 1-16 queries per request. Each `id` must be non-empty and unique.

```json
{
	"start": 1791150000,
	"end": 1791236400,
	"results": [
		{
			"id": "a",
			"unit": "percent",
			"series": [{ "labels": { "name": "nginx" }, "points": [[1791150000, 3.2], [1791150300, null]] }],
			"chart": { "bucket_seconds": 300, "degraded": false, "unavailable": [], "...": "..." }
		},
		{ "id": "b", "unit": "percent", "series": [{ "labels": {}, "points": [[1791236400, 41.7]] }] }
	]
}
```

- Points are `[timestamp, value]`, oldest first. The value is `null` for an
  empty bucket.
- Series are read on the same plan the built-in charts use (tier stitching,
  point budget, coverage). `chart` describes that plan. A keyed query without a
  label returns the busiest `limit` series.
- `stats.avg` is the average over every collector tick in the window, and a
  tick where the key was absent counts as zero. A process outside the top set
  or a stopped container used next to nothing, so a burst that shows in a few
  ticks does not outrank load that ran all day. A bucket's ticks are the raw
  samples of the key that was present most often in it. `min` and `max` are
  bucket means taken over the buckets where the key was present, so a spike
  shorter than one bucket is flattened.
- `unit` is one of `percent`, `bytes`, `bytes/s`, `/s` or `celsius`, and is
  absent for counts and ratios.
- A malformed request fails as a whole with a 400. A query that is well formed
  but wrong fails on its own: its result carries `error` and an empty `series`,
  and the rest of the screen still draws.

## Assistant

`POST /assistant` and the `done` event of `POST /assistant/stream` carry a
`screens` field next to `answer` and `proposals`. It is always present, and
`[]` when the answer composed none.

```json
{ "answer": "...", "proposals": [], "screens": [{ "id": "s1", "screen": { "title": "...", "range": "1h", "root": { "...": "..." } } }] }
```

At most 3 screens per answer, each a canonical spec, with no two the same.
`screens` is not replayed in `history`; only the answer text is.
