/** Adds. */
export function add(a: number, b: number): number {
  const inner = () => 1;
  function helper() {}
  return a + b;
}

export class Greeter extends Base {
  name = "x";
  constructor() {
    super();
  }
  greet(): string {
    return "hi";
  }
  get title() {
    return "t";
  }
}

export interface Shape {
  area(): number;
}

enum Colour {
  Red,
}

type Pair = [number, number];

namespace Util {
  export const LIMIT = 3;
  export function run() {}
}

export const MAX = 10;
let counter = 0;
const arrow = (x: number) => x;
const { d1, d2 } = obj;

@Component({})
export class Decorated {
  @Input() p = 1;
}
