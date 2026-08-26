/** Serializes mutations for a keyed Discord resource and releases after every outcome. */
export class KeyedLock {
  private tails = new Map<string, Promise<void>>();
  async run<T>(key: string, work: () => Promise<T>): Promise<T> {
    const previous = this.tails.get(key) ?? Promise.resolve();
    let release!: () => void;
    const current = new Promise<void>((resolve) => { release = resolve; });
    const chain = previous.then(() => current);
    this.tails.set(key, chain);
    await previous;
    try { return await work(); } finally { release(); if (this.tails.get(key) === chain) this.tails.delete(key); }
  }
}
