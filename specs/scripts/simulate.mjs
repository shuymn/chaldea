// Simulates each spec with fixed seeds and checks every invariant
// (docs/design/formal-specs.md "CI"). Fails on a violation, and when a witness is never
// reached across all seeds of its spec (the simulation stopped covering it).
import { availableParallelism } from "node:os";
import { quint } from "./quint.mjs";
import { FAMILIES } from "./specs.mjs";

const SEEDS = [1, 2, 3, 4];
const MAX_SAMPLES = 1000;
const MAX_STEPS = 40;

const THREAD_INPUT_INVARIANTS = [
  "sendingAtMostOnce",
  "writtenAtMostOnce",
  "sendingOnlyThroughOpenGate",
  "dispatchOnlyThroughRecheck",
  "oneInFlightPerLaunch",
  "earlierLaunchesNeverBlock",
  "terminalStatesImmutable",
  "neverResent",
  "unconfirmedAttentionIff",
  "gateTimeoutOnlyWhileQueued",
  "noStartupCauseAfterBindOrExit",
  "stallOnlyForUpgradedCodex",
  "openCardsAwaitApproval",
  "lateResolutionChangesNothing",
  "restartRecovers",
  "exitedLaunchHasNoPendingInput",
  "ackOnlyWhenAttributable",
  "reservedOnlyAfterDeath",
  "resumeBlockedOnlyWhileDeathUnconfirmed",
  "exitedTurnUnknown",
  "taskOnlyByTaskSet",
  "gateEvidenceIsBound",
  "openGateEvidenceOfCurrentTurn",
  "ackedHasCause",
  "cardKeysOnlyToOpenCards",
  "interruptedOnlyForCodex",
  "supervisorInputsAccepted",
];

const THREAD_INPUT_WITNESSES = [
  "witnessAcked",
  "witnessAckedThroughGate",
  "witnessLateAck",
  "witnessRecheckFailed",
  "witnessDismissed",
  "witnessSecondLaunch",
  "witnessPositionalAcked",
  "witnessResumeBlocked",
  "witnessOpenAfterEarlierUnconfirmed",
  "witnessBufferedPromotion",
  "witnessLateResolutionBesideOpenCard",
  "witnessTwoCardsOpen",
];

// The invariants and witnesses of each model.
const CHECKS = {
  "thread_input.qnt": { invariants: THREAD_INPUT_INVARIANTS, witnesses: THREAD_INPUT_WITNESSES },
  "sync.qnt": {
    invariants: [
      "noGapNoDup",
      "heldEventsOnHost",
      "noMixedEpochs",
      "renderedStateAtCursor",
      "appliedKeepsHeld",
      "olderResponsesChangeNothing",
      "epochOnlyFromCurrentForward",
      "epochNeverRegresses",
      "owedResyncStaysDue",
      "convergedAfterQuiesce",
    ],
    witnesses: [
      "witnessEpochSwitch",
      "witnessConvergedAcrossEpochs",
      "witnessOlderForwardArrived",
      "witnessBackwardApplied",
      "witnessBackwardEpochMismatch",
      "witnessTooManyEvents",
      "witnessTooManyBytes",
      "witnessEpochChanged",
      "witnessHasMore",
      "witnessOnlyHeadsRecover",
      "witnessResubscribeWhilePending",
      "witnessResyncKept",
      "witnessVanishedStream",
      "witnessReadFailedRetried",
    ],
  },
  "receipts.qnt": {
    invariants: [
      "oneIntentPerCommand",
      "oneEffectPerCommand",
      "receiptIffIntent",
      "replayOnlyToCreator",
      "othersLearnNothing",
      "unknownNeverReexecuted",
      "noInProgressAfterRestart",
      "timeoutOnlyWithoutIntent",
      "timeoutOnlyWhenExpiredObserved",
      "inProgressOnlyWithReceipt",
      "replyFromObservedByGrace",
    ],
    witnesses: [
      "witnessReplay",
      "witnessReplayNon200",
      "witnessReplayAfterOutcomeUnknown",
      "witnessDurableUnobserved",
      "witnessInProgress",
      "witnessEffectAfterReply",
      "witnessExpiredAfterGrace",
      "witnessDuplicateWhileInProgress",
      "witnessUnknownAfterCrash",
      "witnessConflict",
      "witnessTimeoutBeforeSubmit",
      "witnessTimeoutThenIntent",
      "witnessCommitUnknownCommitted",
    ],
  },
};

const SPECS = FAMILIES.map(({ model }) => {
  if (!(model in CHECKS)) throw new Error(`no invariants or witnesses listed for ${model}`);
  return { spec: model, ...CHECKS[model] };
});

// Every (spec, seed) pair is one Node process; at most one per available core runs at once.
const started = Date.now();
const runs = SPECS.flatMap((s) => SEEDS.map((seed) => ({ ...s, seed, result: null })));
let next = 0;
async function worker() {
  while (next < runs.length) {
    const run = runs[next++];
    const { spec, seed, invariants, witnesses } = run;
    run.result = await quint([
      "run", spec,
      "--seed", String(seed),
      "--max-samples", String(MAX_SAMPLES),
      "--max-steps", String(MAX_STEPS),
      "--invariants", ...invariants,
      "--witnesses", ...witnesses,
      "--verbosity", "2",
    ], { capture: true });
  }
}
await Promise.all(Array.from({ length: Math.min(availableParallelism(), runs.length) }, worker));
const seconds = ((Date.now() - started) / 1000).toFixed(1);

let failed = false;
const seen = new Map(SPECS.map((s) => [s.spec, Object.fromEntries(s.witnesses.map((w) => [w, 0]))]));
runs.forEach(({ spec, seed, invariants, result: { status, out } }) => {
  if (status !== 0) {
    failed = true;
    console.log(out);
    console.log(`${spec} seed ${seed}: FAILED (exit ${status})`);
    return;
  }
  const counts = seen.get(spec);
  for (const m of out.matchAll(/^(\w+) was witnessed in (\d+) trace/gm)) {
    if (m[1] in counts) counts[m[1]] += Number(m[2]);
  }
  console.log(`${spec} seed ${seed}: ok, ${MAX_SAMPLES} samples x ${MAX_STEPS} steps, ${invariants.length} invariants`);
});
console.log(`${runs.length} runs in ${seconds} s`);
for (const [spec, counts] of seen) {
  for (const [w, n] of Object.entries(counts)) {
    console.log(`${spec} ${w}: ${n} of ${SEEDS.length * MAX_SAMPLES} traces`);
    if (n === 0) failed = true;
  }
}
process.exit(failed ? 1 : 0);
