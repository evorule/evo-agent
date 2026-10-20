// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  clampSidebarWidth,
  clampPanelHeight,
  groupPercentFromDrag,
  SIDEBAR_MIN,
  SIDEBAR_MAX,
  PANEL_MIN,
  GROUP_MIN_PCT,
  GROUP_MAX_PCT,
} from '../src/lib/layout.js';

describe('clampSidebarWidth', () => {
  it('界内取整原样返回', () => {
    expect(clampSidebarWidth(260)).toBe(260);
    expect(clampSidebarWidth(260.7)).toBe(261);
  });

  it('低于下限钳制(180)', () => {
    expect(clampSidebarWidth(10)).toBe(SIDEBAR_MIN);
  });

  it('高于上限钳制(480)', () => {
    expect(clampSidebarWidth(9999)).toBe(SIDEBAR_MAX);
  });

  it('非数值回落下限', () => {
    expect(clampSidebarWidth(NaN)).toBe(SIDEBAR_MIN);
  });
});

describe('clampPanelHeight', () => {
  it('界内原样返回', () => {
    expect(clampPanelHeight(300, 1000)).toBe(300);
  });

  it('低于下限钳制(120)', () => {
    expect(clampPanelHeight(10, 1000)).toBe(PANEL_MIN);
  });

  it('上限 = 50% 视口高', () => {
    expect(clampPanelHeight(9999, 1000)).toBe(500);
  });

  it('视口过矮时下限兜底(上限不低于下限)', () => {
    expect(clampPanelHeight(9999, 100)).toBe(PANEL_MIN);
  });
});

describe('groupPercentFromDrag', () => {
  it('按行宽换算像素增量并取整', () => {
    expect(groupPercentFromDrag(33, 160, 1600)).toBe(43);
  });

  it('低于下限钳制(10%)', () => {
    expect(groupPercentFromDrag(12, -1600, 1600)).toBe(GROUP_MIN_PCT);
  });

  it('高于上限钳制(80%)', () => {
    expect(groupPercentFromDrag(70, 1600, 1600)).toBe(GROUP_MAX_PCT);
  });

  it('行宽非法时回落起点钳制', () => {
    expect(groupPercentFromDrag(75, 100, 0)).toBe(75);
    expect(groupPercentFromDrag(5, 100, undefined)).toBe(GROUP_MIN_PCT);
  });
});
