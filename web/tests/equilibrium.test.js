// 均衡观测呈现映射测试(着色/数据行/灰置三组;纯函数,不触网络)
import { describe, expect, it } from 'vitest';
import { phaseColor, layerRows, attributionRows } from '../src/lib/equilibrium.js';

describe('equilibrium gauge mapping', () => {
  // ---- 着色映射:相位词→语义色;未知值一律灰(防假绿) ----
  it('maps phases to semantic colors, unknown stays gray', () => {
    expect(phaseColor('absorbing')).toBe('balanced');
    expect(phaseColor('relaxing')).toBe('relaxing');
    expect(phaseColor('crisis')).toBe('crisis');
    // 防假绿:缺失/空/未知相位绝不映射成相位色
    expect(phaseColor(undefined)).toBe('unknown');
    expect(phaseColor('')).toBe('unknown');
    expect(phaseColor('BALANCED')).toBe('unknown');
    expect(phaseColor('ok')).toBe('unknown');
  });

  // ---- popover 数据行:三层固定序;缺失层以 unknown 呈现不省略 ----
  it('renders three layer rows in fixed order, missing layers shown as unknown', () => {
    const rows = layerRows({
      layer_phases: { capability: 'crisis', data: 'absorbing', rules: 'relaxing' },
    });
    expect(rows).toEqual([
      { layer: 'capability', phase: 'crisis' },
      { layer: 'rules', phase: 'relaxing' },
      { layer: 'data', phase: 'absorbing' },
    ]);
    // 旧 server 缺 layer_phases 或个别层:行保留,相位 unknown(灰字)
    expect(layerRows({})).toEqual([
      { layer: 'capability', phase: 'unknown' },
      { layer: 'rules', phase: 'unknown' },
      { layer: 'data', phase: 'unknown' },
    ]);
    expect(layerRows({ layer_phases: { capability: 'relaxing' } })[1]).toEqual({
      layer: 'rules',
      phase: 'unknown',
    });
    expect(layerRows(null)).toHaveLength(3);
  });

  // ---- 警量分账摘要行:capability+Σ真域(键序)+total;缺块=空(popover 隐藏段) ----
  it('builds attribution rows with sorted domains, hides section when absent', () => {
    const rows = attributionRows({
      alert_attribution: { capability: 1, domains: { thermal: 2, economic: 3 }, total: 6 },
    });
    expect(rows).toEqual([
      { label: 'capability', value: 1 },
      { label: 'economic', value: 3 },
      { label: 'thermal', value: 2 },
      { label: 'total', value: 6 },
    ]);
    // 不变式:capability+Σdomains==total
    const sum = rows
      .filter((r) => r.label !== 'total')
      .reduce((a, r) => a + r.value, 0);
    expect(sum).toBe(6);
    // 缺分账块(旧 server)/null 数据:空数组,popover 隐藏该段
    expect(attributionRows({})).toEqual([]);
    expect(attributionRows(null)).toEqual([]);
    expect(attributionRows({ alert_attribution: null })).toEqual([]);
    // 空 domains:仅 capability+total 两行
    expect(attributionRows({ alert_attribution: { capability: 0, domains: {}, total: 0 } })).toEqual(
      [
        { label: 'capability', value: 0 },
        { label: 'total', value: 0 },
      ],
    );
  });
});
