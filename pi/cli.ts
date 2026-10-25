import process from "node:process";

/** Writes one JSON value as a single stdout line, the protocol every pi CLI speaks. */
export function outputJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

export async function readStdin(): Promise<string> {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input;
}

/** The stdout line a CLI prints when its main fails. */
export interface InternalFailure {
  status: "error";
  code: "internal";
  message: string;
}

function internalFailure(error: unknown): InternalFailure {
  return {
    status: "error",
    code: "internal",
    message: error instanceof Error ? error.message : String(error),
  };
}

/** Runs a CLI's main; a failure becomes one stdout line and exit code 1. */
export function runMain(main: () => Promise<void>, toFailure: (error: unknown) => unknown = internalFailure): void {
  main().catch((error: unknown) => {
    outputJson(toFailure(error));
    process.exitCode = 1;
  });
}
