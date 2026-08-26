export type LogContext = Record<string, string | number | boolean | undefined>;

function write(level: string, message: string, context: LogContext = {}): void {
  const safe = Object.fromEntries(Object.entries(context).filter(([, value]) => value !== undefined));
  console.log(JSON.stringify({ timestamp: new Date().toISOString(), level, message, ...safe }));
}

export const logger = {
  info: (message: string, context?: LogContext) => write('info', message, context),
  warn: (message: string, context?: LogContext) => write('warn', message, context),
  error: (message: string, context?: LogContext) => write('error', message, context),
};
