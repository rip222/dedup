// component.tsx — JSX element names must be kept verbatim so a
// `<Button />` and a `<Link />` component never alpha-rename to the
// same token stream.

import * as React from "react";
import { useState } from "react";

export function Greeting(props: { name: string }): JSX.Element {
    const [count, setCount] = useState(0);
    return (
        <section>
            <Header title={props.name} />
            <Button onClick={() => setCount(count + 1)}>Click</Button>
            <span>{count}</span>
        </section>
    );
}

function Header(props: { title: string }): JSX.Element {
    return <h1>{props.title}</h1>;
}

function Button(props: { onClick: () => void; children: React.ReactNode }): JSX.Element {
    return <button onClick={props.onClick}>{props.children}</button>;
}
