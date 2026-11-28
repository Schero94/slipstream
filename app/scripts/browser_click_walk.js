// Real browser click walk for the Slipstream shell.
//
// Every other test_*.mjs in this folder is a regex over dist/*. Those cannot see
// whether a click does anything, and three separate times during 2026-08-02/03 a
// static reading disagreed with what the running app did. This walk drives the real
// DOM instead.
//
// Run it through the Playwright skill/MCP with `page` supplied, after serving
// dist/ (for example `python3 -m http.server 4173` from app/dist)
// and installing a __TAURI__ stub via addInitScript.
//
// IMPORTANT: disable the HTTP cache before asserting on edited files, or the page
// keeps an older app.js and the walk reports a fix as broken:
//   const c = await page.context().newCDPSession(page);
//   await c.send('Network.enable');
//   await c.send('Network.setCacheDisabled', { cacheDisabled: true });
//
// Returns { failures: [...], checks: {...} }. Empty failures means the walk passed.

async (page) => {
  const failures = [];
  const checks = {};
  const fail = (what, detail) => failures.push(`${what}: ${detail}`);

  const clickTab = async (tab) => {
    await page.evaluate((t) => document.querySelector(`nav button[data-tab="${t}"]`)?.click(), tab);
    await page.waitForTimeout(200);
  };

  // 1. Every tab renders something and none throws.
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') pageErrors.push(m.text()); });

  const tabs = await page.evaluate(() =>
    [...document.querySelectorAll('nav button.tab[data-tab]')].map((b) => b.dataset.tab));
  checks.tabs = [];
  for (const tab of tabs) {
    await clickTab(tab);
    const info = await page.evaluate(() => {
      const main = document.querySelector('main');
      const vis = [...main.querySelectorAll(':scope > *')]
        .filter((el) => getComputedStyle(el).display !== 'none' && el.offsetHeight > 0);
      const de = document.documentElement;
      return {
        panels: vis.length,
        textLen: vis.map((e) => e.innerText || '').join(' ').trim().length,
        overflowX: de.scrollWidth > de.clientWidth,
      };
    });
    checks.tabs.push({ tab, ...info });
    if (info.panels === 0) fail('empty tab', tab);
    if (info.textLen < 20) fail('tab renders almost nothing', `${tab} (${info.textLen} chars)`);
    if (info.overflowX) fail('horizontal overflow', tab);
  }

  // 2. aria-selected must follow the visual active tab.
  checks.ariaSelected = [];
  for (const tab of tabs) {
    await clickTab(tab);
    const st = await page.evaluate(() => {
      const bs = [...document.querySelectorAll('nav button.tab')];
      return {
        sel: bs.filter((b) => b.getAttribute('aria-selected') === 'true').map((b) => b.dataset.tab),
        vis: bs.filter((b) => b.classList.contains('tab-active')).map((b) => b.dataset.tab),
      };
    });
    const ok = st.sel.length === 1 && st.sel[0] === tab && st.vis[0] === tab;
    checks.ariaSelected.push({ tab, ...st, ok });
    if (!ok) fail('aria-selected out of sync', `clicked ${tab}, announced ${JSON.stringify(st.sel)}`);
  }

  // 3. Path labels must focus their field when clicked.
  await clickTab('models');
  await page.evaluate(() => { const d = document.querySelector('details.paths'); if (d) d.open = true; });
  await page.waitForTimeout(150);
  checks.labelFocus = [];
  for (const id of ['pDir', 'pPgrn', 'pUrl', 'pServer']) {
    const got = await page.evaluate((target) => {
      const l = document.querySelector(`label[for="${target}"]`);
      if (!l) return '(no label)';
      l.click();
      return document.activeElement?.id || document.activeElement?.tagName;
    }, id);
    checks.labelFocus.push({ id, focused: got });
    if (got !== id) fail('label does not focus its input', `${id} -> ${got}`);
  }

  // 4. The shared tools/JSON contract survives backend switches and a reload.
  await clickTab('chat');
  const setBackend = async (v) => {
    await page.evaluate((val) => {
      const s = document.getElementById('backendSel');
      if (s) { s.value = val; s.dispatchEvent(new Event('change', { bubbles: true })); }
    }, v);
    await page.waitForTimeout(250);
  };
  const readContract = () => page.evaluate(() => {
    const t = document.getElementById('chatTools');
    const j = document.getElementById('chatJson');
    const s = document.getElementById('chatSchema');
    return { tools: t?.checked ?? null, json: j?.checked ?? null, schemaLen: (s?.value || '').length };
  });
  await setBackend('mlx');
  await page.evaluate(() => {
    for (const id of ['chatTools', 'chatJson']) {
      const el = document.getElementById(id);
      if (el && !el.checked) { el.checked = true; el.dispatchEvent(new Event('change', { bubbles: true })); }
    }
    const s = document.getElementById('chatSchema');
    if (s) { s.value = '{"type":"object"}'; s.dispatchEvent(new Event('input', { bubbles: true })); }
  });
  await page.waitForTimeout(250);
  const onMlx = await readContract();
  await setBackend('metal');
  const onMetal = await readContract();
  await setBackend('auto');
  const onAuto = await readContract();
  checks.contract = { onMlx, onMetal, onAuto };
  for (const [name, st] of [['metal', onMetal], ['auto', onAuto]]) {
    if (st.tools !== onMlx.tools || st.json !== onMlx.json || st.schemaLen !== onMlx.schemaLen) {
      fail('tools/JSON state lost on backend switch', `mlx=${JSON.stringify(onMlx)} ${name}=${JSON.stringify(st)}`);
    }
  }

  // 5. Toast must be an announced live region.
  checks.toast = await page.evaluate(() => {
    const t = document.getElementById('toast');
    return { role: t?.getAttribute('role'), live: t?.getAttribute('aria-live') };
  });
  if (checks.toast.role !== 'status') fail('toast not a status region', JSON.stringify(checks.toast));
  if (checks.toast.live !== 'polite') fail('toast has no aria-live', JSON.stringify(checks.toast));

  // 6. Your Macs (v0.6): pairing walk against the mock (__M.remote) — MacBook ≤ 3 clicks,
  //    big Mac 1 click, no raw i18n keys, start_server refused while using another Mac.
  checks.macs = {};
  const hasRemote = await page.evaluate(() => !!(window.__M && window.__M.remote));
  if (hasRemote) {
    await page.evaluate(() => {
      const R = window.__M.remote;
      R.mode = 'off'; R.usePeer = null; R.paired = []; R.shareOn = false; R.pairing = null; R.removedBy = null;
      R.useState = 'connected'; R.modelRunning = true;
      R.found = [{ fp: '3f2a9c41d07b5e88', pairing_open: true, addrs: ['/ip4/192.168.1.20/udp/9003/quic-v1'], paired_peer: null, paired_name: null }];
      window.__M.srv = 'down';
    });
    await clickTab('cluster');
    const rawKeys = await page.evaluate(() => [...document.querySelectorAll('[data-panel="cluster"] [data-i18n]')]
      .filter((el) => el.textContent.trim() === el.dataset.i18n).map((el) => el.dataset.i18n));
    if (rawKeys.length) fail('untranslated keys on Your Macs', rawKeys.join(', '));
    let clicks = 0;
    await page.click('#macsFind'); clicks++;
    await page.waitForTimeout(500);
    if (!(await page.isVisible('#macsPairForm'))) {
      // more than one Mac: pick it
      await page.click('#macsFound button'); clicks++;
      await page.waitForTimeout(200);
    }
    await page.fill('#macsCodeIn', '482913');
    await page.press('#macsCodeIn', 'Enter'); clicks++;
    await page.waitForTimeout(2600);
    const pill = await page.evaluate(() => ({ cls: document.getElementById('statusPill').className, text: document.getElementById('statusPill').textContent }));
    checks.macs.macbookClicks = clicks;
    checks.macs.pill = pill;
    if (clicks > 3) fail('MacBook pairing takes more than 3 clicks', String(clicks));
    if (!/pill-remote-on/.test(pill.cls)) fail('pill not in remote-on after pairing', JSON.stringify(pill));
    const startErr = await page.evaluate(async () => {
      try { await window.__TAURI__.core.invoke('start_server', { cfg: {} }); return 'started'; } catch (e) { return String(e); }
    });
    checks.macs.startWhileRemote = startErr;
    if (!/^\[remote_active\]/.test(startErr)) fail('start_server not refused while using another Mac', startErr);
    await page.click('#macsStopUsing');
    await page.waitForTimeout(1300);
    const after = await page.evaluate(() => document.getElementById('statusPill').className);
    if (/pill-remote/.test(after)) fail('pill still remote after Stop using', after);
    // Big Mac: one click shows the code.
    await page.click('#macsShareToggle');
    await page.waitForTimeout(1200);
    const code = await page.evaluate(() => document.getElementById('macsCode6').textContent);
    checks.macs.bigMacCode = code;
    if (!/^\d{3} \d{3}$/.test(code)) fail('Share this Mac did not show a code in one click', code);
    await page.click('#macsShareToggle');
    await page.waitForTimeout(400);
  }

  // 7. Nothing may have thrown along the way.
  checks.pageErrors = pageErrors;
  if (pageErrors.length) fail('console/page errors', pageErrors.slice(0, 3).join(' | '));

  return { failures, checks };
}
