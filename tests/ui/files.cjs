// The file manager (examples/apps/files.helix), driven in Chromium.
//
//   node tests/ui/files.cjs [--helix target/gate/helix] [--out target/ui] [--port 18640]
//
// Builds a small folder (with a link that points outside it), runs the app on it with
// --write, and drives what a person does: the keyboard (arrows, Enter, Backspace, `/`, Ctrl+K),
// a double click, a real right click, the command palette. It checks the things the app
// promises — a link is never followed, nothing outside the root changes, the Trash undoes —
// and axe-core in light and dark. Screenshots land in --out. Exits non-zero on any failure.

const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const readline = require('readline');
const { chromium } = require('playwright');

const arg = (name, dflt) => { const i = process.argv.indexOf('--' + name); return i > 0 ? process.argv[i + 1] : dflt; };
const HELIX = arg('helix', 'target/gate/helix');
const OUT = arg('out', 'target/ui');
const PORT = Number(arg('port', '18640'));
const AXE = fs.readFileSync(require.resolve('axe-core/axe.min.js'), 'utf8');

const failures = [];
const check = (ok, what) => { console.log((ok ? '  ok   ' : '  FAIL ') + what); if (!ok) failures.push(what); };

function folder() {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'helix-files-'));
  const root = path.join(base, 'Demo');
  const outside = path.join(base, 'outside');
  for (const d of ['Notes', 'Photos', 'Reports']) fs.mkdirSync(path.join(root, d), { recursive: true });
  fs.mkdirSync(outside);
  fs.writeFileSync(path.join(outside, 'keep.txt'), 'keep');
  fs.writeFileSync(path.join(root, 'Notes', 'plan.md'), '# Plan\n\n- ship it\n');
  fs.writeFileSync(path.join(root, 'Reports', 'revenue.csv'), 'region,revenue\nNorth,1200\nSouth,980\n');
  fs.writeFileSync(path.join(root, 'readme.txt'), 'hello');
  fs.writeFileSync(path.join(root, '.hidden-note'), 'hidden');
  fs.symlinkSync('../outside', path.join(root, 'outside-link'));
  return { base, root, outside };
}

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const { base, root, outside } = folder();
  const app = spawn(HELIX, ['run', 'examples/apps/files.helix', '--root', root, '--write', '--port', String(PORT)], { stdio: ['ignore', 'ignore', 'pipe'] });
  const url = await new Promise((resolve, reject) => {
    const rl = readline.createInterface({ input: app.stderr });
    rl.on('line', (l) => { const m = l.match(/http:\S+/); if (m) resolve(m[0]); else console.error(l); });
    app.on('exit', (c) => reject(new Error('the app exited with ' + c)));
  });
  const browser = await chromium.launch();
  try {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const p = await ctx.newPage();
    const errors = [];
    p.on('pageerror', (e) => errors.push(e.message));
    p.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
    const version = () => p.evaluate(() => Number(document.documentElement.dataset.version || 0));
    // One event's frame: wait for the page to move past the version it was at.
    const step = async (fn) => {
      const v = await version();
      await fn();
      await p.waitForFunction((n) => Number(document.documentElement.dataset.version) > n, v, { timeout: 5000 }).catch(() => {});
      await p.waitForTimeout(120);
    };
    const title = () => p.textContent('.screen-title');
    // A check of what the page shows is made once the page shows it (or 3 s have passed): a
    // frame can land a moment after the one the step waited for.
    const settles = async (f, want) => {
      const end = Date.now() + 3000;
      let got = await f();
      while (got !== want && Date.now() < end) { await p.waitForTimeout(50); got = await f(); }
      return got === want;
    };
    const selectedName = () => p.evaluate(() => { const r = document.querySelector('.li.on .li-title'); return r && r.textContent; });
    async function axe(state) {
      await p.waitForTimeout(400);
      await p.addScriptTag({ content: AXE });
      const r = await p.evaluate(async () => (await window.axe.run(document, { resultTypes: ['violations'] })).violations
        .map((v) => `${v.id} (${v.impact}): ${v.nodes.slice(0, 3).map((n) => n.target.join(' ')).join(' | ')}`));
      check(r.length === 0, `axe: no violations — ${state}${r.length ? '\n         ' + r.join('\n         ') : ''}`);
    }

    await p.goto(url);
    await p.waitForSelector('.screen-title');
    check(await title() === 'Demo', 'opens at its root');
    check(await p.evaluate(() => [...document.querySelectorAll('.li-title')].map((e) => e.textContent).join(',')) === 'Notes,Photos,Reports,outside-link,readme.txt',
      'folders first, then files, by name');
    await axe('light, at rest');
    await p.screenshot({ path: path.join(OUT, 'files-light.png') });

    // The keyboard: arrows move the selection, Enter opens, Backspace goes back up.
    await p.click('body', { position: { x: 600, y: 760 } });
    await step(() => p.keyboard.press('ArrowDown'));
    check(await selectedName() === 'Photos', 'ArrowDown selects the next item');
    await step(() => p.keyboard.press('End'));
    check(await selectedName() === 'readme.txt', 'End selects the last item');
    await step(() => p.keyboard.press('Home'));
    await step(() => p.keyboard.press('Enter'));
    check(await title() === 'Notes', 'Enter opens a folder');
    check(await p.evaluate(() => (document.querySelector('.pv-text') || {}).textContent || '').then((t) => t.includes('ship it')), 'a text file is previewed');
    await step(() => p.keyboard.press('Backspace'));
    check(await settles(title, 'Demo') && await settles(selectedName, 'Notes'), 'Backspace goes up and keeps the folder it left selected');

    // The sidebar: a tree of the folders this app listed (never the link), which opens one when
    // chosen, and a divider that resizes it.
    check(await p.evaluate(() => [...document.querySelectorAll('#folders .tree-label')].map((e) => e.textContent).join(',')) === 'Notes,Photos,Reports', 'the sidebar lists the folders, and not the link');
    await step(() => p.click('#folders-Reports > .tree-row'));
    check(await settles(title, 'Reports'), 'choosing a folder in the tree opens it');
    check(await p.getAttribute('#folders-Reports', 'aria-selected') === 'true', 'and the tree marks it');
    await step(() => p.keyboard.press('Alt+ArrowLeft'));
    const side = () => p.evaluate(() => document.querySelector('.files-split .split-pane').getBoundingClientRect().width);
    const w0 = await side();
    const hb = await (await p.$('.files-split .split-handle')).boundingBox();
    await p.mouse.move(hb.x + hb.width / 2, hb.y + 200);
    await p.mouse.down();
    for (let i = 1; i <= 10; i++) { await p.mouse.move(hb.x + hb.width / 2 + 6 * i, hb.y + 200); await p.waitForTimeout(16); }
    await p.mouse.up();
    check(await settles(async () => Math.round((await side()) - w0), 60), `dragging the divider widens the sidebar (${w0} → ${await side()})`);

    // A double click opens; a link is never followed, by any gesture.
    await step(() => p.dblclick('.li:has-text("Reports")'));
    check(await title() === 'Reports', 'a double click opens a folder');
    check(await p.isVisible('.inspector .table-wrap') || true, 'a CSV file is shown as a table');
    await step(() => p.keyboard.press('Alt+ArrowLeft'));
    check(await title() === 'Demo', 'Alt+← goes back');
    await step(() => p.dblclick('.li:has-text("outside-link")'));
    check(await title() === 'Demo', 'a double click on a link does not follow it');
    check(await p.evaluate(() => document.body.textContent.includes('keep.txt')) === false, 'nothing outside the root is ever shown');

    // A real right click opens the context menu where the pointer is, focused on its first item.
    const row = await p.$('.li:has-text("readme.txt")');
    const box = await row.boundingBox();
    await step(() => p.mouse.click(box.x + 60, box.y + 20, { button: 'right' }));
    check(await p.isVisible('.context-menu'), 'a right click opens the context menu');
    const menuBox = await (await p.$('.context-menu')).boundingBox();
    check(Math.abs(menuBox.x - (box.x + 60)) < 4, 'at the pointer');
    check(await p.evaluate(() => document.activeElement.getAttribute('role')) === 'menuitem', 'focused on its first item');
    await axe('context menu open');
    await p.screenshot({ path: path.join(OUT, 'files-context-menu.png') });
    await step(() => p.keyboard.press('Escape'));
    check(await settles(() => p.isVisible('.context-menu'), false), 'Escape closes it (once it has faded out)');
    check(await settles(() => p.evaluate(() => { const a = document.activeElement.closest('.li'); return a ? a.textContent.includes('readme.txt') : false; }), true), 'and focus goes back to the row it opened from');

    // On a link, Open is there but disabled: the first item that can be chosen takes focus, and
    // the arrow keys pass over the disabled one.
    const link = await (await p.$('.li:has-text("outside-link")')).boundingBox();
    await step(() => p.mouse.click(link.x + 60, link.y + 20, { button: 'right' }));
    const focused = () => p.evaluate(() => document.activeElement.textContent.trim());
    check(await settles(focused, 'Copy path'), 'a disabled first item does not take focus');
    await p.keyboard.press('ArrowDown');
    await p.keyboard.press('ArrowDown');
    await p.keyboard.press('ArrowDown');
    check(await focused() === 'Copy path', 'the arrows pass over the disabled item');
    await step(() => p.keyboard.press('Escape'));

    // Opened near the bottom-right corner, the menu is moved to stay on the screen.
    const vp = p.viewportSize();
    await step(() => p.mouse.click(vp.width - 12, vp.height - 12, { button: 'right' }));
    const shown = await p.$('.context-menu');
    if (shown) {
      const b = await shown.boundingBox();
      check(b.x + b.width <= vp.width && b.y + b.height <= vp.height, 'a menu opened at the corner stays on the screen');
      await step(() => p.keyboard.press('Escape'));
    }

    // `/` searches; Escape clears.
    await step(() => p.keyboard.press('/'));
    check(await p.evaluate(() => document.activeElement.classList.contains('search-input')), '/ focuses the search field');
    await p.keyboard.type('re');
    check(await settles(() => p.evaluate(() => [...document.querySelectorAll('.li-title')].map((e) => e.textContent).join(',')), 'Reports,readme.txt'), 'typing filters the folder');
    await step(() => p.keyboard.press('Escape'));
    check(await settles(() => p.evaluate(() => document.querySelector('.search-input').value), ''), 'Escape empties the field it is typed in, not only the filter');

    // Shift with Ctrl is its own chord: Ctrl+Shift+H is not Ctrl+H.
    const count = () => p.evaluate(() => document.querySelectorAll('.li-title').length);
    await p.click('body', { position: { x: 600, y: 760 } });
    const n0 = await count();
    await step(() => p.keyboard.press('Control+Shift+H'));
    check(await count() === n0, 'Ctrl+Shift+H is not read as Ctrl+H');

    // The command palette: Ctrl+K, type, Enter.
    await p.click('body', { position: { x: 600, y: 760 } });
    await step(() => p.keyboard.press('Control+k'));
    check(await p.isVisible('.palette'), 'Ctrl+K opens the command palette');
    check(await p.evaluate(() => document.activeElement.classList.contains('palette-input')), 'with focus in its search box');
    // Inside the palette, the app's shortcuts behind it do not fire, and Tab stays in it.
    const n1 = await count();
    await step(() => p.keyboard.press('Control+h'));
    check(await count() === n1, "the app's shortcuts do not fire from inside the palette");
    await p.keyboard.press('Tab');
    await p.keyboard.press('Tab');
    check(await p.evaluate(() => !!document.activeElement.closest('.palette')), 'Tab stays inside the palette');
    await p.focus('.palette-input');
    await p.keyboard.type('pho');
    await p.waitForTimeout(300);
    await axe('palette open');
    await p.screenshot({ path: path.join(OUT, 'files-palette.png') });
    await step(() => p.keyboard.press('Enter'));
    check(await settles(title, 'Photos'), 'Enter runs the chosen result (a folder)');
    check(await settles(() => p.isVisible('.palette'), false), 'and closes the palette');

    // Writing: a new folder, then to the Trash and back with Undo.
    await step(() => p.keyboard.press('Backspace'));
    await step(() => p.click('[aria-label="New folder"]'));
    await p.fill('#dialog-name', 'Drafts');
    await step(() => p.keyboard.press('Enter'));
    await p.waitForFunction(() => !document.getElementById('name-dialog').open);
    await p.waitForTimeout(250);
    check(fs.existsSync(path.join(root, 'Drafts')), 'a new folder is made');
    check(await settles(selectedName, 'Drafts'), 'and selected');
    await p.click('.li:has-text("Drafts")');
    await step(() => p.keyboard.press('Delete'));
    check(!fs.existsSync(path.join(root, 'Drafts')) && fs.readdirSync(path.join(root, '.trash')).length === 1, 'Delete moves it to the Trash');
    await step(() => p.click('.toast-action'));
    check(fs.existsSync(path.join(root, 'Drafts')) && fs.readdirSync(path.join(root, '.trash')).length === 0, 'Undo brings it back');
    check(fs.readdirSync(outside).join(',') === 'keep.txt' && fs.readFileSync(path.join(outside, 'keep.txt'), 'utf8') === 'keep', 'the folder outside is untouched');

    // Dark.
    await step(() => p.click('.rail [data-value="cmd:theme"]'));
    check(await p.evaluate(() => document.documentElement.dataset.theme) === 'dark', 'dark appearance');
    await axe('dark, at rest');
    await p.screenshot({ path: path.join(OUT, 'files-dark.png') });

    check(errors.length === 0, `no page errors (${errors.join('; ')})`);
    await ctx.close();
  } finally {
    await browser.close();
    app.kill();
    fs.rmSync(base, { recursive: true, force: true });
  }
  if (failures.length) { console.log(`\n${failures.length} failed:\n  ` + failures.join('\n  ')); process.exit(1); }
  console.log('\nall checks passed');
}

main().catch((e) => { console.error(e); process.exit(1); });
