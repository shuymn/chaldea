// The spec families, in the order every script runs them: the model, its traces module,
// and the directory of its committed traces (relative to specs/).
export const FAMILIES = [
  { model: "thread_input.qnt", traces: "thread_input_traces.qnt", dir: "traces" },
  { model: "sync.qnt", traces: "sync_traces.qnt", dir: "traces/sync" },
  { model: "receipts.qnt", traces: "receipts_traces.qnt", dir: "traces/receipts" },
];
