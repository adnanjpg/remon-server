# Assistant views

The assistant can answer with widgets as well as text. When an operator asks to
see, chart, watch or compare something, the model calls `propose_view` and the
client renders the widget under the answer from its own widget catalog. The
model picks *what* to show; the data comes from the normal API, so no number in
a view ever passes through the model.

This file is the contract between remon-server and remon-web and lives in both
repos. Change it in both, in the same step.

## Response

`POST /assistant` and the `done` event of `POST /assistant/stream` carry a
`views` array next to `answer` and `proposals`. It is always present, `[]` when
the answer has none.

```json
{
  "answer": "nginx peaked around 14:10; the rest of the window is flat.",
  "proposals": [],
  "views": [
    {
      "id": "v1",
      "title": "CPU, last 6h",
      "config": { "kind": "history-chart", "resource": "cpu", "range": "6h" }
    }
  ]
}
```

| field    | type   | notes                                                  |
| -------- | ------ | ------------------------------------------------------ |
| `id`     | string | `v1`, `v2`, ... unique within one answer only          |
| `title`  | string | model-written caption, trimmed, at most 80 chars       |
| `config` | object | a remon-web `WidgetConfig`, exactly as the dashboard stores it |

Rules the server guarantees:

- At most 6 views per answer, in the order the model added them.
- No two views in one answer have the same `config`.
- Every `config` has passed the validation below. Clients should still run it
  through their own normalizer before rendering. If a client finds a kind it
  does not know (for example, the server is newer than the client), it should
  show a small "unsupported view" card. It should not drop the view silently.

Views are not replayed in `history`. Only the answer text is.

## `config`

The source of truth is `WidgetConfig` in remon-web
`src/lib/types/dashboard.ts`. The value sets come from `RangeKey` in
`src/lib/components/charts/range.ts` and from `LIVE_KPI_SOURCES` in
`src/lib/dashboard/live-kpi.ts`. The server copies them in
`src/assistant/views.rs`.

| kind             | fields                                                        |
| ---------------- | ------------------------------------------------------------- |
| `history-chart`  | `resource`: `cpu` \| `memory` \| `disk` \| `network`; `range`: `30m` \| `1h` \| `6h` \| `24h` \| `7d` \| `30d` |
| `probe-metric`   | `probe`, `metric`: strings; `viz`: `chart` \| `scalar`; optional `unit`, `labelKey` |
| `live-kpi`       | `source`: `cpu` \| `memory` \| `disk-io` \| `network`         |
| `status-summary` | `summary`: `host` \| `services` \| `containers` \| `alerts`   |
| `live-vitals`, `cpu-detail`, `memory-detail`, `pressure`, `network-detail`, `disk-detail`, `alert-timeline` | none |

### `probe-metric`

The model never writes `unit` or `labelKey` itself:

- It passes `labels` (an object such as `{"site": "eu"}`) and the server turns
  it into `labelKey`. That is the canonical JSON of the label set: keys
  sorted, no whitespace, e.g. `{"site":"eu"}`. This is the same encoding
  as `labelKey()` in remon-web `src/lib/utils/probeMetrics.ts` and
  `ProbeMetric::labels_canonical` on the server. If `labels` is left out,
  `labelKey` is absent and the widget shows the most populated series.
- The server fills in `unit` from the probe's latest report.
- `viz` defaults to `chart`.

## Validation (server)

A bad call goes back to the model as a tool error that names the fix. The
model then retries, and the view is not added. A call is rejected for:

- an unknown `kind`. The error lists the valid kinds.
- a missing or out-of-set value. The error lists the valid values.
- a field the kind does not take, e.g. `range` on `cpu-detail`.
- `probe-metric` only:
  - the probe is not registered,
  - the probe has not reported any metrics yet,
  - the metric is not in the probe's latest report,
  - `labels` match none of that metric's label sets.

## Adding a widget kind

1. Add the widget to remon-web: `WidgetConfig`, `normalizeConfig` in
   `src/lib/dashboard/defaults.ts`, and `WidgetHost`.
2. Ship that client before the server starts emitting the new kind. Older
   clients show the "unsupported view" card for it.
3. Add the kind to `views.rs`: the `normalize` arm, the tool schema and the
   tool description. Then update the table above in both repos.
