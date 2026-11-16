// The tooltips of the panels' spans tables (issue 737): one floating
// element serves every bar, segment and event, showing the text its
// data-tip carries, its first line as the name. It opens on hover, on
// keyboard focus and on tap, and closes when the pointer leaves, focus
// moves on, a tap lands elsewhere, Escape is pressed or the page
// scrolls. The toolbar's script carries this and the dashboard's request
// page loads it; in the toolbar the element goes in the toolbar's shadow
// root, where the panels are, and on the dashboard in the page. The text
// is set as text, never as HTML.

(() => {
  "use strict";

  const made = new WeakMap();
  let shown = null;

  // What under the event carries a tooltip, through a shadow root too.

  const carrier = (event) =>
    event.composedPath().find((node) => node instanceof Element && node.hasAttribute("data-tip")) || null;

  const tipFor = (root) => {
    let tip = made.get(root);

    if (!tip) {
      tip = document.createElement("div");
      tip.className = "muru-tip";
      tip.setAttribute("role", "tooltip");
      tip.hidden = true;
      (root instanceof ShadowRoot ? root : document.body).append(tip);
      made.set(root, tip);

      // A scroll inside a shadow root does not reach the window.

      if (root instanceof ShadowRoot) {
        root.addEventListener("scroll", hide, true);
      }
    }

    return tip;
  };

  const hide = () => {
    if (shown) {
      shown.tip.hidden = true;
      shown.node.removeAttribute("aria-describedby");
      shown = null;
    }
  };

  const show = (node) => {
    if (shown && shown.node === node) {
      return;
    }

    hide();
    const tip = tipFor(node.getRootNode());
    const [name, ...lines] = node.getAttribute("data-tip").split("\n");
    const title = document.createElement("strong");
    title.textContent = name;
    tip.replaceChildren(title, ...lines.map((line) => {
      const one = document.createElement("div");
      one.textContent = line;
      return one;
    }));
    tip.id = tip.id || `muru-tip-${Math.random().toString(36).slice(2)}`;
    node.setAttribute("aria-describedby", tip.id);
    tip.hidden = false;

    // Above what it describes, or below when there is no room, and kept
    // within the window's width.

    const box = node.getBoundingClientRect();
    const size = tip.getBoundingClientRect();
    const gap = 6;
    const left = Math.min(Math.max(box.left + box.width / 2 - size.width / 2, gap), window.innerWidth - size.width - gap);
    const above = box.top - size.height - gap;
    tip.style.left = `${Math.max(left, gap)}px`;
    tip.style.top = `${above >= gap ? above : box.bottom + gap}px`;
    shown = { node, tip };
  };

  // Listened for in the page and in the toolbar's shadow root: an event
  // whose related element is in the same shadow root, focus moving from
  // one of its elements to another or the pointer crossing between them,
  // goes no further than that root.

  const toolbar = document.getElementById("muru-toolbar");
  const scopes = [document, ...(toolbar && toolbar.shadowRoot ? [toolbar.shadowRoot] : [])];

  for (const scope of scopes) {
    // Hover follows the pointer's moves; a tooltip whose element has
    // keyboard focus stays when the pointer goes elsewhere.

    scope.addEventListener("pointermove", (event) => {
      if (event.pointerType === "touch") {
        return;
      }

      const node = carrier(event);

      if (node) {
        show(node);
      } else if (shown && (!shown.node.isConnected || !shown.node.matches(":focus"))) {
        hide();
      }
    });

    scope.addEventListener("focusin", (event) => {
      const node = carrier(event);

      if (node) {
        show(node);
      } else {
        hide();
      }
    });

    scope.addEventListener("focusout", (event) => {
      if (shown && carrier(event) === shown.node) {
        hide();
      }
    });

    // A tap on a carrier opens its tooltip; one anywhere else closes it.

    scope.addEventListener("click", (event) => {
      const node = carrier(event);

      if (node) {
        show(node);
      } else {
        hide();
      }
    });

    scope.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        hide();
      }
    });
  }

  document.documentElement.addEventListener("pointerleave", () => {
    if (shown && !shown.node.matches(":focus")) {
      hide();
    }
  });

  window.addEventListener("scroll", hide, true);
})();
