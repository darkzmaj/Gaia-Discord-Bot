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
    method?: unknown;
    url?: unknown;
    route?: unknown;
    message?: unknown;
    stack?: unknown;
    cause?: { code?: unknown; status?: unknown; message?: unknown };
    rawError?: { code?: unknown; message?: unknown };
  };
  const cause = value?.cause;
  const code = value?.code ?? value?.rawError?.code ?? cause?.code;
  const message = String(
    value?.rawError?.message ??
      value?.message ??
      cause?.message ??
      (typeof error === "string" ? error : undefined) ??
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
    status:
      typeof value?.status === "number"
        ? value.status
        : typeof cause?.status === "number"
          ? cause.status
          : undefined,
    method:
      typeof value?.method === "string" ? value.method : undefined,
    route:
      typeof value?.route === "string"
        ? value.route
        : typeof value?.url === "string"
          ? value.url.replace(/^https?:\/\/[^/]+/, "")
          : undefined,
    discordMessage: message,
    stack,
    ...context,
  });
}
