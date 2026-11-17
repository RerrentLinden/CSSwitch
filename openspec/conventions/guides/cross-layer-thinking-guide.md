# Cross-Layer Thinking Guide

> **Purpose**: Think through data flow across layers before implementing.

---

## The Problem

**Most bugs happen at layer boundaries**, not within layers.

Common cross-layer bugs:

- API returns format A, frontend expects format B
- Database stores X, service transforms to Y, but loses data
- Multiple layers implement the same logic differently

---

## Before Implementing Cross-Layer Features

### Step 1: Map the Data Flow

Draw out how data moves:

```
Source → Transform → Store → Retrieve → Transform → Display
```

For each arrow, ask:

- What format is the data in?
- What could go wrong?
- Who is responsible for validation?

### Step 2: Identify Boundaries

| Boundary              | Common Issues                     |
| --------------------- | --------------------------------- |
| API ↔ Service         | Type mismatches, missing fields   |
| Service ↔ Database    | Format conversions, null handling |
| Backend ↔ Frontend    | Serialization, date formats       |
| Component ↔ Component | Props shape changes               |

### Step 3: Define Contracts

For each boundary:

- What is the exact input format?
- What is the exact output format?
- What errors can occur?

---

## Common Cross-Layer Mistakes

### Mistake 1: Implicit Format Assumptions

**Bad**: Assuming date format without checking

**Good**: Explicit format conversion at boundaries

### Mistake 2: Scattered Validation

**Bad**: Validating the same thing in multiple layers

**Good**: Validate once at the entry point

### Mistake 3: Leaky Abstractions

**Bad**: Component knows about database schema

**Good**: Each layer only knows its neighbors

### Mistake 4: Every Consumer Parses The Same Payload

**Bad**: A command reads JSONL events and casts fields inline:

```typescript
const thread = (ev as { thread?: string }).thread;
const labels = (ev as { labels?: string[] }).labels;
```

This looks local, but it means every consumer owns a private version of the
event contract. The next field change will update one command and miss another.

**Good**: Decode once at the event boundary, then export typed projections:

```typescript
if (!isThreadEvent(ev)) return false;
return ev.thread === filter.thread;
```

**Rule**: For append-only logs, JSON streams, RPC payloads, or config files,
create one owner for:

- event / payload type definitions
- type guards and normalization from `unknown`
- metadata projections used by UI commands
- reducers that replay state from the source of truth

Rendering code may format fields, but it must not redefine the payload contract.

---

## Checklist for Cross-Layer Features

Before implementation:

- [ ] Mapped the complete data flow
- [ ] Identified all layer boundaries
- [ ] Defined format at each boundary
- [ ] Decided where validation happens

After implementation:

- [ ] Tested with edge cases (null, empty, invalid)
- [ ] Verified error handling at each boundary
- [ ] Checked data survives round-trip
- [ ] Checked that consumers import shared decoders / projections instead of
      casting payload fields locally
- [ ] Checked that derived state points back to the source event identifier
      (`seq`, `id`, `version`) instead of inventing a second cursor

## 移植他人实现时的额外检查

从其它项目搬语义(不是搬代码)时,多问三句:

- [ ] **判据的边界我自己验过吗?** 对方的条件可能比上游实际拒收的范围宽。
      本项目实测过一次:照搬"任何非 none 的 tool_choice 都禁思考",
      而上游只拒收指定工具型——结果在最普通的形态上白白关掉了推理。
- [ ] **策略的权威来源是契约还是环境变量?** 声明在契约里、却只从环境变量读,
      等于任何忘记注入的启动路径都静默丢掉整条能力。
- [ ] **新组件会不会踩既有应用的文件名?** 同目录下的旧应用可能把不认识的
      schema 当成自己的旧版本去迁移,后果是它清空自己的数据。

---

## When to Create Flow Documentation

Create detailed flow docs when:

- Feature spans 3+ layers
- Multiple teams are involved
- Data format is complex
- Feature has caused bugs before

---

## Event Log / Projection Boundary

Append-only logs are cross-layer contracts. A single event travels through:

```
CLI input → event writer → events.jsonl → reader → filter → reducer → display
```

### Checklist: After Adding A New Event Kind Or Field

- [ ] Add the event kind to the central event taxonomy
- [ ] Add a typed event variant or type guard at the event layer
- [ ] Add normalization helpers for array/object fields that come from
      user input or JSON
- [ ] Keep `seq` / `id` assignment in the event writer only
- [ ] Make filters and reducers consume the typed event guard, not local casts
- [ ] Make display code consume reducer output or typed events, not raw JSON
- [ ] Add at least one regression that proves history replay and live filtering
      use the same filter model

**Real-world example**: Thread channels added `kind: "thread"`, `description`,
`context`, labels, and `lastSeq`. The first implementation replayed thread
state correctly, but several commands still re-parsed event payload fields with
local casts. The fix was to make the core event layer own `ThreadChannelEvent`
and `isThreadEvent`, make `reduceChannelMetadata` the only channel metadata
projection, and make `reduceThreads` the only thread replay reducer.

---

## Provider Channels And Upstream Protocol Adaptation

A provider channel is a cross-layer contract. One channel spans:

```
catalog contract → gateway transform → SSE filter → Science rendering
       ↓                  ↓                              ↑
  tauri validators   capability rules            desktop template + preview mock
```

### Checklist: Before Adding Or Changing A Provider Channel

- [ ] List every registration point first, then change them in one pass —
      missing one surfaces as unrelated-looking failures in another crate
      (see backend quality guidelines, "接入一个新的 provider 渠道")
- [ ] Ask whether the new behaviour must be scoped to one contract; never
      widen an existing provider's branch to cover a new upstream
- [ ] Check whether a sibling channel already has a compensation for the same
      symptom, and whether its shape actually fits — reusing a mismatched one
      costs capability (e.g. disabling thinking for `any` when only `tool` needs it)
- [ ] Register new tests in the source-gate fixture with fully qualified ids

### Checklist: Before Concluding "The Upstream Does Not Support X"

- [ ] Did every source/shape variant fail the same way?
- [ ] Does a deliberately non-existent type produce the identical error?
      (If yes, it is an unknown-type path, not a format mismatch.)
- [ ] Does a sibling provider behave differently on the same request?
- [ ] Did you separate "accepted" from "actually read"? A 200 with a payload
      the model never consults is not support — probe with a unique marker and
      a falsifiable question.

### Checklist: Before Injecting Or Rewriting SSE Content Blocks

- [ ] Does the injection happen inside the filtered stream? The lifecycle
      validator withholds `message_stop` until a clean EOF, so anything
      appended after the forwarding loop reads as a truncated stream.
- [ ] Is every incrementally-streamed field emitted as a delta? Payload placed
      only in `content_block_start` is dropped by spec-compliant clients — and
      the stripped block is then echoed back upstream on the next turn.
- [ ] Do output block indexes stay gapless after swallowing or inserting blocks?
- [ ] Does a failed injection surface as a terminal error rather than
      fabricated assistant content?

**Real-world example**: The Kimi for Coding channel needed four compensations.
Two were found only under real Claude Science traffic: a work-item classifier
that forces one named tool (rejected whenever thinking is on, and thinking is on
by default), and a PDF attachment delivered as a `document` block that the
upstream does not implement — one attachment broke every later turn of the
session. A third defect was self-inflicted: spliced blocks carried their payload
only in `content_block_start`, so the client reconstructed empty blocks and
echoed them back. Unit tests passed throughout; only a spec-compliant
reconstruction test caught it.
