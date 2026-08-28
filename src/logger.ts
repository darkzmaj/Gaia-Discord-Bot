export type LogContext = Record<string, string | number | boolean | undefined>;

function write(level: string, message: string, context: LogContext = {}): void {
  const safe = Object.fromEntries(
    Object.entries(context).filter(([, value]) => value !== undefined),
  );
  console.log(
    JSON.stringify({
      timestamp: new Date().toISOString(),
      level,
      message,
      ...safe,
    }),
  );
}

export const logger = {
  info: (message: string, context?: LogContext) =>
    write("info", message, context),
  warn: (message: string, context?: LogContext) =>
    write("warn", message, context),
  error: (message: string, context?: LogContext) =>
    write("error", message, context),
};

/** Keeps Discord REST diagnostics structured without printing embed content or tokens. */
export function logDiscordError(
  operation: string,
  error: unknown,
  context: LogContext = {},
): void {
  const value = error as {
    code?: unknown;
    status?: unknown;
    message?: unknown;
    stack?: unknown;
    rawError?: { code?: unknown; message?: unknown };
  };
  const code = value?.code ?? value?.rawError?.code;
  const message = String(
    value?.rawError?.message ??
      value?.message ??
      "unknown Discord REST failure",
  )
    .replace(/[\r\n]/g, " ")
    .slice(0, 300);
  const stack =
    process.env.NODE_ENV === "production"
      ? undefined
      : typeof value?.stack === "string"
        ? value.stack.slice(0, 3000)
        : undefined;
  write("error", "[discord] operation failed", {
    operation,
    code: code === undefined ? undefined : String(code),
    status: typeof value?.status === "number" ? value.status : undefined,
    discordMessage: message,
    stack,
    ...context,
  });
}
