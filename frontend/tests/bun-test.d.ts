declare module 'bun:test' {
  type MaybePromise = void | Promise<void>;
  type TestCallback = () => MaybePromise;
  type AnyFunction = (...args: any[]) => any;

  interface MockMetadata<T extends AnyFunction> {
    calls: Array<Parameters<T>>;
  }

  type MockFunction<T extends AnyFunction> = T & {
    mock: MockMetadata<T>;
  };

  interface MockFactory {
    <T extends AnyFunction>(implementation: T): MockFunction<T>;
    restore(): void;
  }

  interface Matchers {
    toEqual(expected: unknown): void;
    toHaveBeenCalledTimes(expected: number): void;
    toHaveBeenCalledWith(...expected: unknown[]): void;
  }

  export const mock: MockFactory;
  export function afterEach(callback: TestCallback): void;
  export function describe(name: string, callback: TestCallback): void;
  export function test(name: string, callback: TestCallback): void;
  export function expect(actual: unknown): Matchers;
}
