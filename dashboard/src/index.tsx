/**
 * ION Dashboard — Ink (React for Terminal)
 */
import React from "react";
import { render } from "ink";
import { App } from "./app";

render(<App />, { exitOnCtrlC: true });
