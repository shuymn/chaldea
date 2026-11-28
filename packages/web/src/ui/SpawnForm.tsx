// The new-thread form (docs/design/w2-composer.md §2.2 "New thread"): agent, cwd, an
// optional first prompt and task, each with a visible label. Empty optionals are omitted
// (an empty prompt is `prompt_rejected`; an empty task would make `task_state` active).
// The prompt textarea keeps the browser's Enter. An invalid submit focuses the first
// invalid field (the browser's constraint validation, `forms` §4).

import type { Agent } from "@chaldea/contracts/generated/Agent";
import type { SpawnArgs } from "@chaldea/contracts/generated/SpawnArgs";
import { useId, useState } from "react";

import { canSpawn, slots } from "../commands/controls";
import { busy, type CmdState } from "../commands/reducer";
import type { State } from "../sync/state";
import { CommandLine } from "./CommandLine";
import type { Ops } from "./ops";

const AGENTS: readonly Agent[] = ["claude", "codex"];

export function SpawnForm(props: { state: State; cmds: CmdState; ops: Ops }) {
  const { state, cmds, ops } = props;
  const id = useId();
  const [agent, setAgent] = useState<Agent>("claude");
  const [cwd, setCwd] = useState("");
  const [prompt, setPrompt] = useState("");
  const [task, setTask] = useState("");
  const ok = canSpawn(state) && !busy(cmds, slots.spawn);
  return (
    <section aria-labelledby={`${id}-h`} className="px-3 py-2">
      <h2 id={`${id}-h`} className="text-lg font-semibold">
        New thread
      </h2>
      <form
        className="spawn-form flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const args: SpawnArgs = { agent, cwd };
          if (prompt !== "") args.prompt = prompt;
          if (task !== "") args.task = task;
          ops.spawn(args);
        }}
      >
        <label htmlFor={`${id}-agent`}>Agent</label>
        <select id={`${id}-agent`} className="field" value={agent} onChange={(e) => setAgent(e.target.value as Agent)}>
          {AGENTS.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
        </select>
        <label htmlFor={`${id}-cwd`}>Working directory (absolute path)</label>
        <input
          id={`${id}-cwd`}
          className="field"
          type="text"
          required
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          value={cwd}
          onChange={(e) => setCwd(e.target.value)}
        />
        <label htmlFor={`${id}-prompt`}>First prompt (optional)</label>
        <textarea
          id={`${id}-prompt`}
          className="field composer-text"
          rows={2}
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
        />
        <label htmlFor={`${id}-task`}>Task (optional)</label>
        <input id={`${id}-task`} className="field" type="text" value={task} onChange={(e) => setTask(e.target.value)} />
        <div>
          <button type="submit" className="control" disabled={!ok}>
            Start thread
          </button>
        </div>
      </form>
      <CommandLine cmds={cmds} slot={slots.spawn} ops={ops} />
    </section>
  );
}
