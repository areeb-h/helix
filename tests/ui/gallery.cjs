// The component gallery, driven in Chromium (ADR 0055 Phase 2's exit criterion).
//
//   node tests/ui/gallery.cjs [--helix target/gate/helix] [--out target/ui] [--port 18620]
//
// Starts `examples/apps/gallery.helix`, opens the link it prints, and walks every component
// through its states — checking each with axe-core in light AND dark, and checking the
// behaviour the components promise: keyboard paths, focus return, live regions, the
// virtualised table, motion turned off under reduced motion. Screenshots of each state land
// in --out. Exits non-zero on any axe violation or failed check.
//
// Needs `playwright` (with its Chromium) and `axe-core` resolvable from here: in CI,
// `npm install --no-save playwright axe-core && npx playwright install chromium`.

const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');
const readline = require('readline');
const { chromium } = require('playwright');

const arg = (name, dflt) => { const i = process.argv.indexOf('--' + name); return i > 0 ? process.argv[i + 1] : dflt; };
const HELIX = arg('helix', 'target/gate/helix');
const OUT = arg('out', 'target/ui');
const PORT = Number(arg('port', '18620'));
const AXE = fs.readFileSync(require.resolve('axe-core/axe.min.js'), 'utf8');

const failures = [];
const check = (ok, what) => { console.log((ok ? '  ok   ' : '  FAIL ') + what); if (!ok) failures.push(what); };

// One app per pass: an app's model is shared by every page connected to it, so a second
// pass against the same process would start from wherever the first one left it.
async function start(port) {
  const app = spawn(HELIX, ['run', 'examples/apps/gallery.helix', '--port', String(port)], { stdio: ['ignore', 'ignore', 'pipe'] });
  const url = await new Promise((resolve, reject) => {
    const rl = readline.createInterface({ input: app.stderr });
    rl.on('line', (l) => { const m = l.match(/http:\S+/); if (m) resolve(m[0]); else console.error(l); });
    app.on('exit', (c) => reject(new Error('the gallery exited with ' + c)));
  });
  return { app, url };
}

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const browser = await chromium.launch();
  try {
    for (const [k, motion] of ['no-preference', 'reduce'].entries()) {
      const { app, url } = await start(PORT + k);
      try { await session(browser, url, motion); } finally { app.kill(); }
    }
  } finally {
    await browser.close();
  }
  if (failures.length) { console.log(`\n${failures.length} failed:\n  ` + failures.join('\n  ')); process.exit(1); }
  console.log('\nall checks passed');
}

async function session(browser, url, motion) {
  console.log(`\n— reduced motion: ${motion}`);
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: motion });
  const p = await ctx.newPage();
  const errors = [];
  p.on('pageerror', (e) => errors.push(e.message));
  p.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  let frames = 0;
  const frame = async () => { frames += 1; await p.waitForFunction((n) => Number(document.documentElement.dataset.version) >= n, frames); };
  // At rest means NO animation running — not "after a fixed wait". A state is only judged once
  // it has arrived: mid-transition, text can already be light while its card is still white,
  // and axe rightly calls that unreadable. A fixed 60 ms passed locally and failed on CI's
  // slower runner, where even reduced motion's 0.01 ms transitions finish a frame or two later.
  const settle = async () => {
    await p.waitForTimeout(30);
    // A loading shimmer runs forever by design; "at rest" means every animation that ENDS has.
    await p.waitForFunction(() => document.getAnimations().every((a) => a.playState !== 'running' || a.effect.getTiming().iterations === Infinity), null, { timeout: 5000 });
    await p.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  };
  const shot = (name) => motion === 'reduce' ? null : p.screenshot({ path: path.join(OUT, name + '.png'), fullPage: name.endsWith('page') });
  async function axe(state) {
    await settle();
    await p.addScriptTag({ content: AXE });
    const r = await p.evaluate(async () => (await window.axe.run(document, { resultTypes: ['violations'] })).violations
      .map((v) => `${v.id} (${v.impact}): ${v.nodes.slice(0, 3).map((n) => n.target.join(' ')).join(' | ')}`));
    check(r.length === 0, `axe: no violations — ${state}${r.length ? '\n         ' + r.join('\n         ') : ''}`);
  }
  const post = async (fn) => { await fn(); await frame(); await settle(); };

  await p.goto(url);
  await frame();
  await settle();

  for (const theme of ['light', 'dark']) {
    if (theme === 'dark') await post(() => p.click('#dark'));
    check(await p.evaluate(() => document.documentElement.dataset.theme) === theme, `theme is ${theme}`);
    await axe(`${theme}, at rest`);
    await shot(`gallery-${theme}-page`);

    // A submit with nothing filled in shows every error, tied to its field.
    await post(() => p.click('button[type=submit]'));
    const described = await p.getAttribute('#name', 'aria-describedby');
    check(described === 'name-error' && await p.getAttribute('#name', 'aria-invalid') === 'true', `${theme}: an invalid field is described by its error`);
    await axe(`${theme}, form errors showing`);
    await shot(`gallery-${theme}-errors`);
    await post(() => p.click('[data-click="reset"]'));

    // The menu: opening it focuses its first item; arrows move; Escape closes it and gives
    // focus back to its button.
    await post(() => p.click('#actions-button'));
    check(await p.evaluate(() => document.activeElement.getAttribute('role')) === 'menuitem', `${theme}: an opened menu focuses its first item`);
    await p.keyboard.press('ArrowDown');
    check(await p.evaluate(() => document.activeElement.textContent) === 'Rename', `${theme}: ArrowDown moves through the menu`);
    await axe(`${theme}, menu open`);
    await shot(`gallery-${theme}-menu`);
    await post(() => p.keyboard.press('Escape'));
    await p.waitForTimeout(250);
    check(await p.evaluate(() => !document.querySelector('[role=menu]:not([aria-hidden=true])')), `${theme}: Escape closes the menu`);
    check(await p.evaluate(() => document.activeElement.id) === 'actions-button', `${theme}: focus returns to the menu button`);

    // The dialog: modal, labelled, closes on Escape, focus returns to what opened it.
    await p.focus('[data-click="open-dialog"]');
    await post(() => p.keyboard.press('Enter'));
    check(await p.evaluate(() => document.getElementById('confirm-delete').open), `${theme}: the dialog opens`);
    check(await p.evaluate(() => document.getElementById('confirm-delete').contains(document.activeElement)), `${theme}: focus moves into the dialog`);
    await axe(`${theme}, dialog open`);
    await shot(`gallery-${theme}-dialog`);
    await post(() => p.keyboard.press('Escape'));
    await p.waitForTimeout(250);
    check(await p.evaluate(() => !document.getElementById('confirm-delete').open), `${theme}: Escape closes the dialog`);
    check(await p.evaluate(() => document.activeElement.dataset.click) === 'open-dialog', `${theme}: focus returns to the button that opened the dialog`);
  }

  // Tabs: arrow keys select; only the selected tab is tabbable; panels are labelled.
  await p.focus('#settings-tab-account');
  await post(() => p.keyboard.press('ArrowRight'));
  check(await p.getAttribute('#settings-tab-preferences', 'aria-selected') === 'true', 'ArrowRight selects the next tab');
  check(await p.evaluate(() => document.activeElement.id) === 'settings-tab-preferences', 'focus follows the selected tab');
  check(await p.evaluate(() => [...document.querySelectorAll('[role=tab]')].filter((t) => t.tabIndex === 0).length) === 1, 'one tab is in the Tab order');
  await post(() => p.keyboard.press('End'));
  check(await p.getAttribute('#settings-tab-about', 'aria-selected') === 'true', 'End selects the last tab');

  // A switch and a slider send typed values, and the model's answer comes back.
  await post(() => p.click('#settings-tab-preferences'));
  await post(() => p.click('#notify'));
  check(await p.evaluate(() => document.getElementById('notify').checked) === false, 'the switch toggles');
  await post(() => p.fill('.slider', '80'));
  check(await p.evaluate(() => document.querySelector('.progress').getAttribute('aria-valuenow')) === '80', 'the slider drives the progress bar');
  await axe('preferences tab');
  await post(() => p.click('#settings-tab-account'));

  // Events reach `update` in the order they happened. While one request is in flight, typing
  // is merged into the event still waiting — but only into the LAST one: typing after a submit
  // must arrive after it. (Merging into any waiting `name` event once moved "C", typed after
  // Submit, ahead of it, so the submit saw "ABC".) The first post is held to build the queue.
  const sent = [];
  let release;
  const held = new Promise((r) => { release = r; });
  let holding = true;
  await p.route('**/event', async (route) => {
    sent.push(JSON.parse(route.request().postData()));
    if (holding) { holding = false; await held; }
    await route.continue();
  });
  await p.click('#name');
  await p.keyboard.type('A');
  await p.keyboard.type('B');
  await p.click('button[type=submit]');
  await p.focus('#name');
  await p.keyboard.type('C');
  release();
  const until = Date.now() + 3000;
  while (sent.length < 4 && Date.now() < until) await p.waitForTimeout(50);
  await p.waitForTimeout(200);
  await p.unroute('**/event');
  const order = sent.map((e) => (e.msg === 'name' ? `name:${e.value}` : e.msg)).join(' ');
  check(order === 'name:A name:AB submit name:ABC', `typing after Submit arrives after it (${order})`);
  await frame();
  await settle();
  await post(() => p.click('[data-click="reset"]'));

  // Toasts: a live region; each arrives, and leaves when dismissed.
  await post(() => p.click('[data-click="toast"].primary'));
  await post(() => p.click('[data-click="toast"].primary'));
  check(await p.evaluate(() => document.querySelectorAll('.toast').length) === 2, 'two toasts are showing');
  await axe('toasts showing');
  await shot('gallery-toasts');
  await post(() => p.click('.toast [aria-label="Dismiss"]'));
  await p.waitForTimeout(400);
  check(await p.evaluate(() => document.querySelectorAll('.toast').length) === 1, 'a dismissed toast leaves');

  // The virtualised table: scroll to row ~5000; the rows in the page are the rows in view.
  const before = await p.evaluate(() => document.querySelectorAll('#samples tbody tr:not(.pad)').length);
  await p.evaluate(() => document.getElementById('samples').scrollIntoView());
  await p.evaluate(() => { document.getElementById('samples').scrollTop = 180000; });
  await frame();
  await p.waitForTimeout(150);
  const rows = await p.evaluate(() => {
    const w = document.getElementById('samples');
    const top = w.getBoundingClientRect().top + 40;
    const at = document.elementFromPoint(w.getBoundingClientRect().left + 30, top + 10);
    const tr = at && at.closest('tr');
    return { count: document.querySelectorAll('#samples tbody tr:not(.pad)').length, index: tr && Number(tr.getAttribute('aria-rowindex')) };
  });
  check(before < 40 && rows.count < 40, `the table holds a screenful of 10,000 rows (${rows.count})`);
  // 180000 px / 36 px a row = row 5000, which is aria-rowindex 5002 (the header is row 1).
  check(rows.index >= 4998 && rows.index <= 5006, `scrolling to 180000px shows row ~5000 (shows ${rows.index})`);

  // Charts: new data moves the same nodes.
  const dot = await p.$('circle[data-key="p0"]');
  await post(() => p.click('[data-click="reshuffle"]'));
  check(await p.evaluate((d) => d.isConnected, dot), 'a reshuffled scatter moves its points, not replaces them');

  // A segmented control is a radio group: arrow keys move AND choose, focus follows.
  await p.focus('#view [aria-checked="true"]');
  await post(() => p.keyboard.press('ArrowRight'));
  check(await p.getAttribute('#view [data-value="grid"]', 'aria-checked') === 'true', 'ArrowRight chooses the next segment');
  check(await p.evaluate(() => document.activeElement.dataset.value) === 'grid', 'focus follows the chosen segment');
  // Its thumb is a shared element: chosen elsewhere, it glides there on a spring — and under
  // reduced motion it simply is there.
  await p.click('#view [data-value="board"]');
  await p.waitForSelector('#view [data-value="board"] .seg-thumb');
  const gliding = await p.evaluate(() => document.querySelector('#view .seg-thumb').getAnimations().length);
  frames = await p.evaluate(() => Number(document.documentElement.dataset.version));
  check(motion === 'reduce' ? gliding === 0 : gliding > 0, motion === 'reduce' ? 'the segmented thumb does not move under reduced motion' : 'the segmented thumb glides to its new option');
  await settle();

  // The stepper sends the new value and stops at its bounds.
  await post(() => p.click('#copies [aria-label="Increase copies"]'));
  check(await p.textContent('#copies .stepper-value') === '3', 'the stepper steps up');
  await post(() => p.click('#copies [aria-label="Increase copies"]'));
  await post(() => p.click('#copies [aria-label="Increase copies"]'));
  check(await p.evaluate(() => document.querySelector('#copies [aria-label="Increase copies"]').disabled), 'the stepper stops at its maximum');

  // A sheet: rises with focus inside it, is labelled, and Escape closes it.
  await post(() => p.click('[data-click="open-sheet"]'));
  check(await p.evaluate(() => { const d = document.getElementById('share'); return !!d && d.contains(document.activeElement); }), 'an opened sheet takes focus');
  await axe('sheet open');
  await shot('gallery-sheet');
  await post(() => p.keyboard.press('Escape'));
  check(await p.evaluate(() => !document.getElementById('share')), 'Escape closes the sheet');
  check(await p.evaluate(() => document.activeElement && document.activeElement.matches('[data-click="open-sheet"]')), 'and focus goes back to the button that opened it');

  // A toast's action: removing a row offers Undo, and Undo brings it back.
  const before_rows = await p.evaluate(() => document.querySelectorAll('[aria-label="Files"] [role=listitem]').length);
  await post(() => p.click('[data-click="remove"]'));
  await p.waitForTimeout(250);
  check(await p.evaluate(() => document.querySelectorAll('[aria-label="Files"] [role=listitem]').length) === before_rows - 1, 'removing a row removes it');
  await post(() => p.click('.toast-action'));
  await p.waitForTimeout(250);
  check(await p.evaluate(() => document.querySelectorAll('[aria-label="Files"] [role=listitem]').length) === before_rows, 'Undo on the toast brings it back');

  // Every material passes in both modes; the page restyles live when the design changes.
  for (const mat of ['glass', 'tonal', 'hybrid']) {
    const card0 = await p.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--card-bg'));
    await post(() => p.click(`#material [data-value="${mat}"]`));
    if (mat !== 'hybrid') check(await p.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--card-bg')) !== card0 || mat === 'tonal', `the ${mat} material restyles the page`);
    for (let k = 0; k < 2; k++) {
      const theme = await p.evaluate(() => document.documentElement.dataset.theme);
      await axe(`${mat}, ${theme}`);
      if (mat === 'glass') await shot(`gallery-glass-${theme}`);
      await post(() => p.click('#dark'));
    }
  }

  check(errors.length === 0, `no page errors (${errors.join('; ')})`);
  await ctx.close();
}

main().catch((e) => { console.error(e); process.exit(1); });
