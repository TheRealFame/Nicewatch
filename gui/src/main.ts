import { mount } from "svelte";
import App from "./App.svelte";
import Prompt from "./Prompt.svelte";
import "./style.css";

// The game-detection confirmation is a small dedicated window pointing at
// the same bundle with `?prompt=...` in the query string.
const params = new URLSearchParams(window.location.search);
const target = params.get("prompt") === "1" ? Prompt : App;

const app = mount(target, {
  target: document.getElementById("app")!,
});

export default app;