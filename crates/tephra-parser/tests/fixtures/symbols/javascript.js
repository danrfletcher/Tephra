/** Adds. */
function add(a, b) {
  const inner = () => 1;
  return a + b;
}

class Greeter extends Base {
  constructor() {
    super();
  }
  greet() {
    return "hi";
  }
  static make() {
    return new Greeter();
  }
}

const MAX = 10;
let counter = 0;
var legacy = 1;
const arrow = (x) => x;
const fn = function () {};

module.exports = { add };
