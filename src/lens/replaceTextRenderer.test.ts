import { describe, expect, it } from 'vitest'
import {
  layoutReplaceTextFlow,
  normalizeReplaceParagraph,
} from './replaceTextLayout'
import { calibrateReplaceFontPx, replaceSlotContentBox } from './replaceTextRenderer'
import type { LensReplaceRenderSlot } from '../api/tauri'

const widthMeasure = (text: string, fontPx: number) => Array.from(text).length * fontPx * 0.5

function canvasWithInkRatio(ratio = 0.8) {
  const context = {
    font: '',
    measureText(text: string) {
      const fontPx = Number.parseFloat(this.font) || 16
      return {
        width: Array.from(text).length * fontPx * 0.5,
        actualBoundingBoxAscent: fontPx * ratio * 0.8,
        actualBoundingBoxDescent: fontPx * ratio * 0.2,
        actualBoundingBoxLeft: 0,
      }
    },
  }
  return context as unknown as CanvasRenderingContext2D
}

function slot(flow: LensReplaceRenderSlot['flow']): LensReplaceRenderSlot {
  return {
    id: 'r0-s00',
    groupId: 'r0',
    leafIds: ['s0'],
    bounds: { x: 10, y: 20, width: 220, height: 34 },
    anchor: { x: 14, y: 22, baselineY: 42 },
    flow,
    kind: flow === 'paragraph_flow' ? 'paragraph' : 'line',
    align: 'left',
    verticalAlign: 'top',
    sourceFontPx: 20,
    sourceColor: '#111827',
  }
}

describe('replacement translation source typography', () => {
  it('converts source ink height to Canvas em size instead of rendering it smaller', () => {
    const context = canvasWithInkRatio(0.8)
    expect(calibrateReplaceFontPx(context, 20, '中文 translation')).toBeCloseTo(25, 5)
  })

  it('does not cap high-DPI or heading text at 48px', () => {
    const layout = layoutReplaceTextFlow(
      '标题',
      [{ width: 400, height: 140, maxLines: 1 }],
      96,
      widthMeasure,
    )
    expect(layout.complete).toBe(true)
    expect(layout.fontPx).toBeGreaterThan(90)
  })

  it('keeps one source baseline per paragraph slot even when translated text is shorter', () => {
    const layout = layoutReplaceTextFlow(
      '第一行第二行第三行第四行',
      [
        { width: 65, height: 60, maxLines: 1 },
        { width: 65, height: 60, maxLines: 1 },
        { width: 65, height: 60, maxLines: 1 },
      ],
      20,
      widthMeasure,
    )
    expect(layout.complete).toBe(true)
    expect(layout.slots.every(item => item.lines.length <= 1)).toBe(true)
    expect(layout.slots.flatMap(item => item.lines).join('')).toBe('第一行第二行第三行第四行')
  })
})

describe('replacement translation paragraph normalization', () => {
  it('treats model/OCR line breaks as soft wraps for a paragraph', () => {
    expect(normalizeReplaceParagraph('时空卷积通常无法学习\n视频中的运动动态，\n因此需要有效的运动表示。'))
      .toBe('时空卷积通常无法学习视频中的运动动态，因此需要有效的运动表示。')
  })

  it('keeps a normal space when joining Latin paragraph wraps', () => {
    expect(normalizeReplaceParagraph('motion representation\nfor video understanding'))
      .toBe('motion representation for video understanding')
  })

  it('uses the source ink anchor as the available left edge for line flow', () => {
    const box = replaceSlotContentBox(slot('paragraph_flow'))
    expect(box.width).toBe(215)
    expect(box.maxLines).toBe(1)
  })
})
