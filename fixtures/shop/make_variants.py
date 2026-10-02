#!/usr/bin/env python3
"""Regenerates the regression variants of index.html (run after editing it)."""

import os

os.chdir(os.path.dirname(os.path.abspath(__file__)))
base = open("index.html", encoding="utf-8").read()


def variant(name, *edits):
    s = base
    for old, new in edits:
        assert old in s, f"{name}: {old!r} not found"
        s = s.replace(old, new, 1)
    open(name, "w", encoding="utf-8").write(s)


# Buttons renamed: locators break, self-healing should fix them.
variant(
    "v2-renamed.html",
    ('<button type="submit">登录</button>', '<button type="submit">立即登录</button>'),
    ('<button id="checkout" disabled>去结算</button>', '<button id="checkout" disabled>提交订单</button>'),
)
# Real bug: only the first item is charged; assertions must catch it.
variant(
    "v3-bug.html",
    ("  const total = cart.reduce((s, i) => s + i.price, 0);\n  $('#order-info')",
     "  const total = cart[0].price; // BUG: only charges the first item\n  $('#order-info')"),
)
# New console error after login: diagnostics must catch it.
variant(
    "v4-console.html",
    ("function enterShop(user) {\n",
     "function enterShop(user) {\n  console.error('pricing: fallback price table used'); // new regression\n"),
)
