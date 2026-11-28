// The composer's Enter rule (docs/design/w2-composer.md §2.1, D2, D3), as one pure
// decision over the keydown's fields, so each guard is tested alone (`composer.keys`).

/** The window after `compositionend` within which an Enter still belongs to the IME (ms). */
export const COMPOSITION_WINDOW_MS = 50;

export interface KeyFacts {
  readonly key: string;
  readonly repeat: boolean;
  readonly shift: boolean;
  /** Ctrl or Cmd (Meta) held. */
  readonly ctrlOrMeta: boolean;
  readonly isComposing: boolean;
  readonly keyCode: number;
  /** Held from `compositionstart` to `compositionend`. */
  readonly composingFlag: boolean;
  /** The keydown's `timeStamp`, and the last `compositionend`'s (null if none since blur or thread switch). */
  readonly timeStamp: number;
  readonly lastCompositionEnd: number | null;
  /** `(pointer: fine)` matches. */
  readonly finePointer: boolean;
}

/**
 * - `ime`: the Enter belongs to the IME; do nothing and leave the default alone (D3).
 * - `newline`: leave it to the browser (a newline, or any other key's own action).
 * - `submit`: `preventDefault`, then submit.
 * - `ignore`: `preventDefault` and do nothing (a held submit shortcut repeating).
 */
export type KeyDecision = "ime" | "newline" | "submit" | "ignore";

export function decide(k: KeyFacts): KeyDecision {
  if (k.key !== "Enter") return "newline";
  // 1. Composing, by any of four signals.
  if (k.isComposing || k.keyCode === 229 || k.composingFlag) return "ime";
  if (k.lastCompositionEnd !== null && Math.abs(k.timeStamp - k.lastCompositionEnd) < COMPOSITION_WINDOW_MS) {
    return "ime";
  }
  // 2. Shift+Enter: a native newline, repeating natively too.
  if (k.shift) return "newline";
  // 3. The submit shortcuts; plain Enter submits only on a fine primary pointer.
  if (k.ctrlOrMeta || k.finePointer) return k.repeat ? "ignore" : "submit";
  return "newline";
}
