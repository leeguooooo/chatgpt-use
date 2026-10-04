// Runs the page probes from src/channel.rs against DOM fixtures of each ChatGPT
// renderer we know of. Offline: jsdom, no browser, no chatgpt.com.
// Run through scripts/check-dom.sh, which dumps the probes first.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { JSDOM } from "jsdom";

const dir = process.argv[2];
const probe = (name) => fs.readFileSync(path.join(dir, `${name}.js`), "utf8");

function page(html) {
  const dom = new JSDOM(`<body>${html}</body>`, {
    runScripts: "outside-only",
    url: "https://chatgpt.com/c/11111111-2222-3333-4444-555555555555",
  });
  // jsdom has no layout, so no innerText; textContent is close enough here.
  dom.window.eval(
    "Object.defineProperty(HTMLElement.prototype,'innerText',{get(){return this.textContent}})",
  );
  return (name) => JSON.parse(dom.window.eval(probe(name)));
}

const cases = [];
const test = (name, fn) => cases.push([name, fn]);

test("author-role renderer", () => {
  const run = page(`
    <article data-turn-id="t1" data-turn-id-container="t1"><div data-message-author-role="user">q1</div></article>
    <article data-turn-id="t2" data-turn-id-container="t2"><div data-message-author-role="assistant">a1</div></article>
    <article data-turn-id="t3" data-turn-id-container="t3"><div data-message-author-role="user">q2</div></article>
    <article data-turn-id="t4" data-turn-id-container="t4"><div data-message-author-role="assistant">a2</div></article>`);
  assert.deepEqual(run("user_turns"), { count: 2, ids: ["t1", "t3"], known: ["t1", "t2", "t3", "t4"] });
  assert.equal(run("assistant_count"), 2);
  assert.equal(run("last_assistant"), "a2");
  const st = run("state");
  assert.equal(st.user_count, 2);
  assert.equal(st.atext, "a2");
  assert.equal(st.convo, "11111111-2222-3333-4444-555555555555");
});

test("one reply split over sections sharing a turn id is read whole", () => {
  const run = page(`
    <section data-turn-id="u1"><div data-message-author-role="user">q</div></section>
    <section data-turn-id="r1"><div data-message-author-role="assistant">part one</div></section>
    <section data-turn-id="r1"><div data-message-author-role="assistant">part two</div></section>`);
  assert.equal(run("last_assistant"), "part one\n\npart two");
});

test("a reused turn id does not pull in the previous question's reply", () => {
  const run = page(`
    <section data-turn-id="u1"><div data-message-author-role="user">q1</div></section>
    <section data-turn-id="r"><div data-message-author-role="assistant">old</div></section>
    <section data-turn-id="u2"><div data-message-author-role="user">q2</div></section>
    <section data-turn-id="r"><div data-message-author-role="assistant">new</div></section>`);
  assert.equal(run("last_assistant"), "new");
});

test("just submitted: a new user turn and no reply yet reads no reply text", () => {
  const run = page(`
    <section data-turn-id="u1"><div data-message-author-role="user">q1</div></section>
    <section data-turn-id="r1"><div data-message-author-role="assistant">old</div></section>
    <section data-turn-id="u2"><div data-message-author-role="user">q2</div></section>`);
  assert.equal(run("user_turns").count, 2);
  assert.equal(run("assistant_count"), 1);
  assert.equal(run("last_assistant"), "");
});

test("grouped renderer: one turn-key per exchange, no author roles", () => {
  const run = page(`
    <div data-turn-key="k1"><div data-user-message-bubble>hello</div><div data-conversation-role="assistant">one</div></div>
    <div data-turn-key="k2"><div data-user-message-bubble>q2</div>
      <div data-conversation-role="assistant">part A</div><div data-conversation-role="assistant">part B</div></div>`);
  assert.deepEqual(run("user_turns"), { count: 2, ids: ["group:k1", "group:k2"], known: ["group:k1", "group:k2"] });
  assert.equal(run("assistant_count"), 2);
  assert.equal(run("last_assistant"), "part A\n\npart B");
});

test("grouped renderer: a turn with only the question has no reply", () => {
  const run = page(`
    <div data-turn-key="k1"><div data-user-message-bubble>hello</div><div data-conversation-role="assistant">one</div></div>
    <div data-turn-key="k2"><div data-user-message-bubble>q2</div></div>`);
  assert.equal(run("user_turns").count, 2);
  assert.equal(run("assistant_count"), 1);
  assert.equal(run("state").atext, "", "the previous reply is not this question's");
});

test("search-unit renderer inside a turn-key, role only in the unit key", () => {
  const run = page(`
    <div data-turn-key="k1">
      <div data-content-search-unit-key="m1:user"><div data-user-message-bubble><div class="whitespace-pre-wrap">q</div></div></div>
      <div data-content-search-unit-key="m2:assistant"><div class="markdown">the answer</div></div>
    </div>`);
  assert.deepEqual(run("user_turns").ids, ["group:k1"]);
  assert.equal(run("assistant_count"), 1);
  assert.equal(run("last_assistant"), "the answer");
});

test("search-unit rows outside any turn-key", () => {
  const run = page(`
    <div data-chatgpt-search-unit-key="a:user">q1</div>
    <div data-chatgpt-search-unit-key="b:assistant">r1</div>
    <div data-chatgpt-search-unit-key="c:user">q2</div>`);
  assert.deepEqual(run("user_turns"), { count: 2, ids: ["unit:a:user", "unit:c:user"], known: [] });
  assert.equal(run("assistant_count"), 1);
});

test("nested role markers count and read once", () => {
  const run = page(`
    <div data-turn-key="k1"><div data-user-message-bubble><div data-message-author-role="user">q</div></div>
      <div data-conversation-role="assistant" data-content-search-unit-key="x:assistant">
        <div data-message-author-role="assistant">only once</div></div></div>`);
  assert.equal(run("user_turns").count, 1);
  assert.equal(run("assistant_count"), 1);
  assert.equal(run("last_assistant"), "only once");
});

test("a grouped turn never reads the user's own words as the reply", () => {
  const run = page(`
    <div data-turn-key="k1"><div data-user-message-bubble>secret question</div>
      <div data-chatgpt-agent-turn-start></div></div>`);
  assert.equal(run("assistant_count"), 1);
  assert.equal(run("last_assistant"), "");
});

test("the reply is cut at the last user turn even with no turn ids", () => {
  const grouped = page(`
    <div data-turn-key="k1"><div data-user-message-bubble>q1</div><div data-conversation-role="assistant">old</div></div>
    <div data-turn-key="k2"><div data-user-message-bubble>q2</div></div>`);
  assert.equal(grouped("last_assistant"), "", "no new reply yet: neither the old reply nor q2");
  assert.equal(grouped("assistant_count"), 1);
  const legacy = page(`
    <div data-message-author-role="user">q1</div><div data-message-author-role="assistant">old</div>
    <div data-message-author-role="user">q2</div>`);
  assert.equal(legacy("last_assistant"), "");
  assert.equal(legacy("state").atext, "");
});

test("raw search units with no turn ids: a stale reply is cut, a new one read whole", () => {
  const stale = page(`
    <div data-chatgpt-search-unit-key="a:user">q1</div>
    <div data-chatgpt-search-unit-key="b:assistant">stale answer</div>
    <div data-content-search-unit-key="c:user">q2</div>`);
  assert.equal(stale("last_assistant"), "");
  assert.equal(stale("state").atext, "");
  const fresh = page(`
    <div data-chatgpt-search-unit-key="a:user">q1</div>
    <div data-chatgpt-search-unit-key="b:assistant">stale answer</div>
    <div data-content-search-unit-key="c:user">q2</div>
    <div data-content-search-unit-key="d:assistant">new part 1</div>
    <div data-content-search-unit-key="e:assistant">new part 2</div>`);
  assert.equal(fresh("last_assistant"), "new part 1\n\nnew part 2");
});

test("repeated wrappers around one reply keep all of its text", () => {
  const run = page(`
    <div data-turn-key="k1"><div data-user-message-bubble>q</div>
      <div data-conversation-role="assistant"><div data-content-search-unit-key="m:assistant">
        <div class="markdown"><p>first para</p></div><div class="markdown"><p>second para</p></div>
      </div></div>
      <div data-conversation-role="assistant"><div class="markdown">tail</div></div></div>`);
  assert.equal(run("assistant_count"), 1);
  const text = run("last_assistant");
  for (const part of ["first para", "second para", "tail"]) assert.ok(text.includes(part), text);
  assert.equal(text.split("first para").length, 2, "read once: " + text);
});

test("virtualized turns stay known by their container", () => {
  const run = page(`
    <div data-turn-id-container="old1"></div>
    <article data-turn-id="t2" data-turn-id-container="t2"><div data-message-author-role="user">q</div></article>`);
  assert.deepEqual(run("user_turns"), { count: 1, ids: ["t2"], known: ["old1", "t2"] });
});

test("streaming vs done: each stop-button form is seen and clickable", () => {
  for (const button of [
    '<button type="button" data-testid="stop-button">x</button>',
    '<button type="button" data-testid="composer-stop-button">x</button>',
    '<button type="button" aria-label="Stop streaming">x</button>',
  ]) {
    const run = page(`<div data-turn-key="k"><div data-user-message-bubble>q</div></div><form>${button}</form>`);
    assert.equal(run("state").stop, true, button);
    assert.equal(run("click_stop").clicked, true, button);
  }
  const done = page(`<div data-turn-key="k"><div data-user-message-bubble>q</div>
    <div data-conversation-role="assistant">done</div></div><form><button data-testid="send-button">s</button></form>`);
  assert.equal(done("state").stop, false);
  assert.equal(done("click_stop").clicked, false);
});

test("a running tool chip is active; the reply's own prose is not", () => {
  const chip = page(`<div data-turn-key="k"><div data-user-message-bubble>q</div><button>Running tests</button></div>`);
  assert.equal(chip("state").tool_active, true);
  const prose = page(`<div data-turn-key="k"><div data-user-message-bubble>q</div>
    <div data-conversation-role="assistant"><button>Running cargo test</button></div></div>`);
  assert.equal(prose("state").tool_active, false);
});

test("rate-limit dialog in each language is detected and dismissed", () => {
  for (const [text, ok] of [
    ["Too many requests. You're making requests too quickly.", "Got it"],
    ["请求过多 太多请求", "知道了"],
    ["リクエストが多すぎます", "閉じる"],
    ["요청이 너무 많습니다", "알겠습니다"],
  ]) {
    const run = page(`<div role="dialog">${text}<button>${ok}</button></div><div id="prompt-textarea"></div>`);
    assert.deepEqual(run("composer"), { composer: true, limited: true }, text);
    assert.equal(run("state").limited, true, text);
    assert.equal(run("dismiss_dialog").ok, true, text);
  }
  const quiet = page(`<div role="dialog">Okay, here is the answer<button>Look</button></div>`);
  assert.equal(quiet("state").limited, false);
  assert.equal(quiet("dismiss_dialog").ok, false);
});

let failed = 0;
for (const [name, fn] of cases) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}\n     ${e.message.split("\n").join("\n     ")}`);
  }
}
console.log(`${cases.length - failed}/${cases.length} DOM fixtures passed`);
process.exit(failed ? 1 : 0);
