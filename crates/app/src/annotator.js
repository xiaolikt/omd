/* Cursor 风格的元素标注层：shadow DOM overlay，hover 高亮、点击 pin。
 * 数据经 console.log('[omd-pins]' + json) 回传宿主（CEF on_console_message）。
 * 宿主用 browser::set_pick 开关拾取模式。幂等：重复注入直接返回。 */
(() => {
  if (window.__omdAnnotator) return;
  window.__omdPins = [];
  let pick = false;

  const host = document.createElement('div');
  host.style.cssText = 'all:initial;position:fixed;inset:0;z-index:2147483647;pointer-events:none;';
  const root = host.attachShadow({ mode: 'open' });
  root.innerHTML =
    '<style>' +
    '.hl{position:fixed;border:2px solid #4f8cff;border-radius:3px;pointer-events:none}' +
    '.pin{position:fixed;background:#4f8cff;color:#fff;font:11px/1.5 sans-serif;' +
    'padding:2px 7px;border-radius:9px;transform:translate(-50%,-130%);' +
    'pointer-events:auto;cursor:pointer;white-space:nowrap}' +
    '.ed{position:fixed;display:flex;gap:4px;pointer-events:auto;' +
    'transform:translate(-50%,-100%);z-index:1}' +
    '.ed input{font:12px sans-serif;padding:3px 7px;border:1px solid #4f8cff;' +
    'border-radius:6px;outline:none;min-width:160px}' +
    '.ed button{font:11px sans-serif;border:0;border-radius:6px;' +
    'background:#ff5663;color:#fff;cursor:pointer}' +
    '</style>' +
    '<div class="hl" hidden></div><div class="layer"></div>';
  document.documentElement.appendChild(host);
  const hl = root.querySelector('.hl');
  const layer = root.querySelector('.layer');

  const cssPath = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const parts = [];
    let node = el;
    while (node && node.nodeType === 1 && parts.length < 5) {
      let part = node.tagName.toLowerCase();
      if (node.id) {
        parts.unshift('#' + CSS.escape(node.id));
        break;
      }
      const parent = node.parentElement;
      if (parent) {
        const same = Array.from(parent.children).filter(
          (c) => c.tagName === node.tagName,
        );
        if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(node) + 1) + ')';
      }
      parts.unshift(part);
      node = parent;
    }
    return parts.join(' > ');
  };

  const sixStyles = (el) => {
    const s = getComputedStyle(el);
    return {
      color: s.color,
      background: s.backgroundColor,
      fontSize: s.fontSize,
      fontWeight: s.fontWeight,
      display: s.display,
      position: s.position,
    };
  };

  const emit = () => {
    try {
      console.log('[omd-pins]' + JSON.stringify(window.__omdPins));
    } catch (e) {
      /* ignore */
    }
  };

  const drawPin = (pin) => {
    const el = document.createElement('div');
    el.className = 'pin';
    el.textContent = pin.comment
      ? '#' + pin.id + ' ' + pin.tag + ' — ' + pin.comment
      : '#' + pin.id + ' ' + pin.tag;
    el.style.left = pin.box.x + pin.box.w / 2 + 'px';
    el.style.top = pin.box.y + 'px';
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      openEditor(pin);
    });
    layer.appendChild(el);
  };

  document.addEventListener(
    'mouseover',
    (e) => {
      if (!pick) return;
      const r = e.target.getBoundingClientRect();
      Object.assign(hl.style, {
        left: r.x + 'px',
        top: r.y + 'px',
        width: r.width + 'px',
        height: r.height + 'px',
      });
      hl.hidden = false;
    },
    true,
  );

  const insideOverlay = (e) => {
    const path = e.composedPath ? e.composedPath() : [e.target];
    return path.includes(host);
  };
  document.addEventListener(
    'click',
    (e) => {
      if (!pick || insideOverlay(e)) return;
      e.preventDefault();
      e.stopPropagation();
      const el = e.target;
      const r = el.getBoundingClientRect();
      window.__omdPins.push({
        id: window.__omdPins.length + 1,
        selector: cssPath(el),
        tag: el.tagName.toLowerCase(),
        text: (el.innerText || el.value || '').trim().slice(0, 200),
        box: {
          x: Math.round(r.x),
          y: Math.round(r.y),
          w: Math.round(r.width),
          h: Math.round(r.height),
        },
        styles: sixStyles(el),
      });
      const fresh = window.__omdPins[window.__omdPins.length - 1];
      drawPin(fresh);
      emit();
      openEditor(fresh);
    },
    true,
  );

  let editor = null;
  const closeEditor = () => {
    if (editor) { editor.remove(); editor = null; }
  };
  const openEditor = (pin) => {
    closeEditor();
    editor = document.createElement('div');
    editor.className = 'ed';
    editor.style.left = pin.box.x + pin.box.w / 2 + 'px';
    editor.style.top = pin.box.y + 'px';
    const input = document.createElement('input');
    input.value = pin.comment || '';
    input.placeholder = 'comment… (Enter 保存)';
    const del = document.createElement('button');
    del.textContent = '✕';
    del.title = '删除 pin';
    input.addEventListener('keydown', (ev) => {
      ev.stopPropagation();
      if (ev.key === 'Enter') {
        pin.comment = input.value.trim();
        emit();
        closeEditor();
        window.__omdSetPick(pick);
      } else if (ev.key === 'Escape') {
        closeEditor();
      }
    });
    del.addEventListener('click', (ev) => {
      ev.stopPropagation();
      window.__omdPins = window.__omdPins.filter((p) => p.id !== pin.id);
      closeEditor();
      emit();
      window.__omdSetPick(pick);
    });
    editor.appendChild(input);
    editor.appendChild(del);
    layer.appendChild(editor);
    input.focus();
  };

  window.__omdSetPick = (on) => {
    pick = !!on;
    hl.hidden = !pick;
    layer.innerHTML = '';
    window.__omdPins.forEach(drawPin);
    emit();
  };
  window.__omdAnnotator = true;
  console.log('[omd-ready]');
})();
