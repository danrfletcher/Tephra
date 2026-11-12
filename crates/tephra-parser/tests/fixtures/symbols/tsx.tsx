interface Props {
  label: string;
}

/** A button. */
export function Button({ label }: Props) {
  const onClick = () => {};
  return <button onClick={onClick}>{label}</button>;
}

export class Card {
  render() {
    return <div />;
  }
}

const SIZE = 2;
type Id = string;
