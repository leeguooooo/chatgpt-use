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
  // jsdom has no execCommand: record what the probe asked for, against which node.
  dom.window.eval(`document.execCommand = (cmd, ui, value) => {
    (window.__exec = window.__exec || []).push([cmd, value === undefined ? null : value]); return true; };`);
  const run = (name) => JSON.parse(dom.window.eval(probe(name)));
  run.doc = dom.window.document;
  run.window = dom.window;
  return run;
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
  for (const sendBtn of [
    '<button data-testid="send-button">s</button>',
    '<button data-testid="fruitjuice-send-button">s</button>',
    '<button aria-label="Send prompt">s</button>',
    '<button aria-label="发送">s</button>',
  ]) {
    const done = page(`<div data-turn-key="k"><div data-user-message-bubble>q</div>
      <div data-conversation-role="assistant">done</div></div><form>${sendBtn}</form>`);
    assert.equal(done("state").stop, false, sendBtn);
    assert.equal(done("click_stop").clicked, false, sendBtn);
  }
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

const stamped = (run) => [...run.doc.querySelectorAll("[data-cgu-composer]")];

test("composer: the ProseMirror box with #prompt-textarea", () => {
  const run = page(`<form><div id="prompt-textarea" contenteditable="true">draft</div></form>`);
  assert.equal(run("composer").composer, true);
  assert.deepEqual(stamped(run).map(n => n.id), ["prompt-textarea"]);
});

test("composer: the Lexical editor with no #prompt-textarea at all", () => {
  const run = page(`<form data-chatgpt-composer>
    <div contenteditable="true" data-lexical-editor="true" role="textbox">a b</div></form>`);
  assert.equal(run("composer").composer, true);
  assert.equal(stamped(run).length, 1);
  assert.equal(run("composer_fingerprint").n, 2, "reads the Lexical editor's text");
  const ins = run("insert_hello");
  assert.equal(ins.ok, true, JSON.stringify(ins));
  assert.deepEqual(JSON.parse(JSON.stringify(run.window.__exec.at(-1))), ["insertText", "hello"]);
  assert.equal(run("clear_composer").ok, true);
});

test("composer: a hidden kept page's stale editor is never chosen", () => {
  const run = page(`
    <div data-app-shell-page-surface style="display:none"><form><div id="prompt-textarea" contenteditable="true">old</div></form></div>
    <div data-app-shell-page-surface><form data-chatgpt-composer>
      <div contenteditable="true" data-lexical-editor="true" role="textbox" id="live">new</div></form></div>`);
  assert.equal(run("composer").composer, true);
  assert.deepEqual(stamped(run).map(n => n.id), ["live"]);
  assert.equal(run("composer_fingerprint").n, 3, "the visible one's text, not the stale one's");
});

test("composer: a wrapper and its inner editor resolve to the inner one", () => {
  const run = page(`<form><div id="prompt-textarea"><div contenteditable="true" data-lexical-editor="true" id="inner"></div></div></form>`);
  assert.equal(run("composer").composer, true);
  assert.deepEqual(stamped(run).map(n => n.id), ["inner"]);
});

test("composer: two visible editors are ambiguous and read as none", () => {
  const run = page(`
    <form><div id="prompt-textarea" contenteditable="true"></div></form>
    <form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox"></div></form>`);
  assert.equal(run("composer").composer, false);
  assert.equal(stamped(run).length, 0);
  assert.equal(run("insert_hello").error, "composer not found");
  assert.equal(run.window.__exec, undefined, "nothing typed anywhere");
});

test("composer: an edit box inside a conversation turn is not the composer", () => {
  const run = page(`
    <div data-turn-key="k"><div data-user-message-bubble><form data-chatgpt-composer>
      <div contenteditable="true" role="textbox" data-lexical-editor="true">editing q</div></form></div></div>
    <form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox" id="main"></div></form>`);
  assert.deepEqual(stamped(run).length ? [stamped(run)[0].id] : [], []);
  assert.equal(run("composer").composer, true);
  assert.deepEqual(stamped(run).map(n => n.id), ["main"]);
});

test("composer: the stamp follows the composer and never lingers", () => {
  const run = page(`<form><div id="prompt-textarea" contenteditable="true"></div></form>`);
  run("composer");
  run.doc.querySelector("#prompt-textarea").closest("form").style.display = "none";
  run.doc.querySelector("form").setAttribute("hidden", "");
  assert.equal(run("composer").composer, false);
  assert.equal(stamped(run).length, 0, "the hidden node lost its stamp");
});

test("composer: no editor at all (a page still loading)", () => {
  const run = page(`<main>loading</main>`);
  assert.deepEqual(run("composer"), { composer: false, limited: false });
});

/// jsdom has no layout: give every element a 10x10 box at the top-left,
/// except anything inside [hidden], which gets an empty one.
function withLayout(run) {
  run.window.eval(`HTMLElement.prototype.getBoundingClientRect = function () {
    const hidden = !!this.closest('[hidden]');
    return {top: 0, left: 0, width: hidden ? 0 : 10, height: hidden ? 0 : 10};
  };`);
  return run;
}

test("picker: found by its test id, before any structural guess", () => {
  const run = withLayout(page(`<form data-chatgpt-composer>
    <button data-testid="composer-plus-btn" aria-haspopup="menu">+</button>
    <button aria-haspopup="menu">Tools</button>
    <button data-testid="model-switcher-dropdown-button" aria-haspopup="menu">5.6 Sol</button></form>`));
  const p = run("find_picker");
  assert.equal(p.ok, true, JSON.stringify(p));
  assert.equal(p.via, "testid");
  assert.equal(p.label, "5.6 Sol");
});

test("picker: the old composer-row rule still works as a fallback", () => {
  const run = withLayout(page(`<form>
    <button data-testid="composer-plus-btn" aria-haspopup="menu">+</button>
    <button aria-haspopup="menu">Instant</button></form>`));
  const p = run("find_picker");
  assert.equal(p.ok, true, JSON.stringify(p));
  assert.equal(p.via, "row");
});

test("picker: two visible candidates of a kind are not guessed between", () => {
  const run = withLayout(page(`
    <button data-testid="model-switcher-dropdown-button" aria-haspopup="menu">a</button>
    <button data-testid="model-switcher-dropdown-button" aria-haspopup="menu">b</button>`));
  assert.equal(run("find_picker").ok, false);
});

test("picker menu: open when the content is a plain test-id container, not role=menu", () => {
  const run = withLayout(page(`<div data-testid="composer-intelligence-picker-content">
    <div data-model-reasoning-effort-slider><div role="slider" aria-valuenow="2" aria-valuemin="0" aria-valuemax="4"></div></div>
    <div role="menuitemradio" aria-checked="true">Latest</div></div>`));
  const m = run("picker_menu");
  assert.equal(m.open, true);
  assert.equal(m.slider.now, 2);
  assert.equal(m.slider.max, 4);
  assert.deepEqual(JSON.parse(JSON.stringify(m.radios)), [{ text: "Latest", checked: true }]);
});

test("picker menu: the slider inside the picker wins over any other slider", () => {
  const run = withLayout(page(`
    <div role="slider" aria-valuenow="9" aria-valuemin="0" aria-valuemax="9"></div>
    <div role="group"><div data-model-picker-power-slider>
      <div role="slider" aria-valuenow="1" aria-valuemin="0" aria-valuemax="4"></div></div></div>`));
  const m = run("picker_menu");
  assert.equal(m.open, true);
  assert.equal(m.slider.now, 1);
});

test("picker menu: closed when nothing is open", () => {
  const run = withLayout(page(`<form><button aria-haspopup="menu">Instant</button></form>`));
  assert.equal(run("picker_menu").open, false);
});

test("mention menu: exactly one row titled as the app, badge allowed, highlight read", () => {
  const run = page(`<div data-mention-list-scroll-area>
    <button data-list-navigation-item="true">chatgpt-use-old\nlegacy</button>
    <button data-list-navigation-item="true" data-highlighted>Gmail\nmail</button>
    <button data-list-navigation-item="true">chatgpt-use DEV\nLocal project tools</button></div>`);
  const m = run("mention_menu");
  assert.equal(m.count, 1, JSON.stringify(m));
  assert.equal(m.highlighted, false, "Gmail is highlighted, not ours");
  run.doc.querySelectorAll("button")[1].removeAttribute("data-highlighted");
  run.doc.querySelectorAll("button")[2].setAttribute("aria-current", "true");
  assert.equal(run("mention_menu").highlighted, true);
});

test("mention menu: the other row markup, and no match / two matches", () => {
  const one = page(`<div role="listbox"><div class="__menu-item" tabindex="0">chatgpt-use</div></div>`);
  assert.equal(one("mention_menu").count, 1);
  const none = page(`<div role="listbox"><div class="__menu-item" tabindex="0">chatgpt-user</div><div class="__menu-item" tabindex="0">Drive</div></div>`);
  const m = none("mention_menu");
  assert.equal(m.count, 0);
  assert.deepEqual(JSON.parse(JSON.stringify(m.titles)), ["chatgpt-user", "Drive"]);
  const two = page(`<div role="menu"><div class="__menu-item" tabindex="0">chatgpt-use</div><div class="__menu-item" tabindex="0">chatgpt-use DEV</div></div>`);
  assert.equal(two("mention_menu").count, 2, "ambiguous: not chosen");
  const hidden = page(`<div hidden role="listbox"><div class="__menu-item" tabindex="0">chatgpt-use</div></div>`);
  assert.equal(hidden("mention_menu").count, 0);
});

test("mention menu: the sidebar's __menu-item rows are never read as the menu", () => {
  // As seen live: no popup open, sidebar entries share the class.
  const run = page(`<nav aria-label="Chat history">
    <a class="__menu-item" tabindex="0">New chat</a><a class="__menu-item" tabindex="0">Search</a>
    <div class="group __menu-item" tabindex="0">chatgpt-use</div></nav>
    <div class="__menu-item" tabindex="0">chatgpt-use</div>`);
  const m = run("mention_menu");
  assert.equal(m.count, 0, JSON.stringify(m));
  assert.deepEqual(JSON.parse(JSON.stringify(m.titles)), [], "outside a popup nothing counts");
});

test("connector pill: found in the composer by either markup", () => {
  for (const pill of [
    `<span data-id="plugin:asdk_app_1" data-keyword="chatgpt-use">chatgpt-use</span>`,
    `<span app-mention-path="app://x" app-mention-display-name="chatgpt-use" contenteditable="false">chatgpt-use</span>`,
  ]) {
    const run = page(`<form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox">${pill} </div></form>`);
    assert.equal(run("connector_pill").ok, true, pill);
  }
  const other = page(`<form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox">
    <span data-id="plugin:x" data-keyword="Gmail">Gmail</span></div></form>`);
  const r = other("connector_pill");
  assert.equal(r.ok, false);
  assert.deepEqual(JSON.parse(JSON.stringify(r.pills)), ["Gmail"]);
});

test("fingerprint: a connector pill is not message text", () => {
  const run = page(`<form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox">
    <span data-id="plugin:a" data-keyword="chatgpt-use" contenteditable="false">chatgpt-use</span> ab</div></form>`);
  assert.equal(run("composer_fingerprint").n, 2, "only 'ab' counts");
  const bare = page(`<form data-chatgpt-composer><div contenteditable="true" data-lexical-editor="true" role="textbox">ab</div></form>`);
  assert.equal(run("composer_fingerprint").h, bare("composer_fingerprint").h, "same hash as the text alone");
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
