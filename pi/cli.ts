import process from "node:process";

/** Writes one JSON value as a single stdout line, the protocol every pi CLI speaks. */
export function outputJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

/** Reads all of stdin as UTF-8. The chunks are joined before decoding, since a
 * pipe read can end in the middle of a multibyte character. */
export async function readStdin(input: AsyncIterable<Buffer | string> = process.stdin): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of input) chunks.push(typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk);
  return Buffer.concat(chunks).toString("utf8");
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
