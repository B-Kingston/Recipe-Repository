import process from "node:process";

/** Writes one JSON value as a single stdout line, the protocol every pi CLI speaks. */
export function outputJson(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

export async function readStdin() {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input;
}

function internalFailure(error) {
  return {
    status: "error",
    code: "internal",
    message: error instanceof Error ? error.message : String(error),
  };
}

/** Runs a CLI's main; a failure becomes one stdout line and exit code 1. */
export function runMain(main, toFailure = internalFailure) {
  main().catch((error) => {
    outputJson(toFailure(error));
    process.exitCode = 1;
  });
}
