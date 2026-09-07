import { describe, expect, it } from 'vitest'
import {
  layoutReplaceTextFlow,
  normalizeReplaceParagraph,
  type ReplaceTextFlowLayout,
} from './replaceTextLayout'
import {
  calibrateReplaceFontPx,
  compactReplaceParagraphRenderSlots,
  replaceGroupSourceInkPx,
  replaceParagraphContinuationShift,
  replaceSlotContentBox,
} from './replaceTextRenderer'
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

function slot(flow: LensReplaceRenderSlot['flow'], index = 0): LensReplaceRenderSlot {
  return {
    id: `r0-s${String(index).padStart(2, '0')}`,
    groupId: 'r0',
    leafIds: [`s${index}`],
    bounds: { x: 10, y: 20 + index * 42, width: 220, height: 34 },
    anchor: { x: 14, y: 22 + index * 42, baselineY: 42 + index * 42 },
    flow,
    kind: flow === 'paragraph_flow' ? 'paragraph' : 'line',
    align: 'left',
    verticalAlign: 'top',
    sourceFontPx: 20,
    sourceColor: '#111827',
  }
}

function slotAt(flow: LensReplaceRenderSlot['flow'], index: number, y: number, groupId = 'r0') {
  const base = slot(flow, index)
  const dy = y - base.bounds.y
  return {
    ...base,
    id: `${groupId}-s${String(index).padStart(2, '0')}`,
    groupId,
    bounds: { ...base.bounds, y },
    anchor: {
      ...base.anchor,
      y: base.anchor.y + dy,
      baselineY: base.anchor.baselineY + dy,
    },
  }
}

function fakeLayout(lineFlags: boolean[]): ReplaceTextFlowLayout {
  return {
    fontPx: 30,
    lineHeight: 35.4,
    safeScale: 1,
    complete: true,
    slots: lineFlags.map(hasLine => ({
      lines: hasLine ? ['译文'] : [],
      contentWidth: hasLine ? 60 : 0,
      contentHeight: hasLine ? 35.4 : 0,
    })),
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

  it('lets paragraph text grow toward the source line box instead of leaving blank source rows', () => {
    const paragraphSlots = Array.from({ length: 9 }, (_, index) => slot('paragraph_flow', index))
    const context = canvasWithInkRatio(0.8)
    const reportedInk = 20
    const geometryInk = replaceGroupSourceInkPx(paragraphSlots)
    expect(geometryInk).toBeCloseTo(31.28, 2)

    const oldFontPx = calibrateReplaceFontPx(context, reportedInk, '中文段落')
    const newFontPx = calibrateReplaceFontPx(context, geometryInk, '中文段落')
    expect(newFontPx).toBeGreaterThan(oldFontPx * 1.4)

    const text = '译'.repeat(110)
    const oldLayout = layoutReplaceTextFlow(
      text,
      paragraphSlots.map(replaceSlotContentBox),
      oldFontPx,
      widthMeasure,
    )
    const newLayout = layoutReplaceTextFlow(
      text,
      paragraphSlots.map(replaceSlotContentBox),
      newFontPx,
      widthMeasure,
    )
    const usedOldRows = oldLayout.slots.filter(item => item.lines.length > 0).length
    const usedNewRows = newLayout.slots.filter(item => item.lines.length > 0).length
    expect(newLayout.complete).toBe(true)
    expect(usedNewRows).toBeGreaterThan(usedOldRows)
    expect(usedNewRows).toBeGreaterThanOrEqual(8)
  })

  it('does not geometry-boost standalone exact-line labels', () => {
    expect(replaceGroupSourceInkPx([slot('exact_line')])).toBe(20)
  })
})

describe('replacement translation paragraph compaction', () => {
  it('removes a detector outlier gap inside one continuous paragraph', () => {
    const paragraphSlots = [
      slotAt('paragraph_flow', 0, 20),
      slotAt('paragraph_flow', 1, 62),
      slotAt('paragraph_flow', 2, 180),
      slotAt('paragraph_flow', 3, 222),
    ]
    const compacted = compactReplaceParagraphRenderSlots(
      paragraphSlots,
      fakeLayout([true, true, true, true]),
    )

    expect(compacted.map(item => item.anchor.y)).toEqual([22, 64, 106, 148])
  })

  it('pulls the next body paragraph up by unused translated source rows while preserving its real gap', () => {
    const previousSource = [
      slotAt('paragraph_flow', 0, 20),
      slotAt('paragraph_flow', 1, 62),
      slotAt('paragraph_flow', 2, 104),
      slotAt('paragraph_flow', 3, 146),
      slotAt('paragraph_flow', 4, 188),
    ]
    const previousLayout = fakeLayout([true, true, true, false, false])
    const previousRendered = compactReplaceParagraphRenderSlots(previousSource, previousLayout)
    const currentSource = [
      slotAt('paragraph_flow', 0, 242, 'r1'),
      slotAt('paragraph_flow', 1, 284, 'r1'),
    ]

    const shift = replaceParagraphContinuationShift({
      sourceSlots: previousSource,
      renderedSlots: previousRendered,
      layout: previousLayout,
      groupShift: 0,
    }, currentSource)

    expect(shift).toBe(-84)
    const shiftedCurrentTop = currentSource[0].bounds.y + shift
    const previousRenderedBottom = previousRendered[2].bounds.y + previousRendered[2].bounds.height
    expect(shiftedCurrentTop - previousRenderedBottom).toBe(20)
  })

  it('collapses an oversized OCR gap when one source sentence was falsely split into two groups', () => {
    const previousSource = [
      slotAt('paragraph_flow', 0, 20),
      slotAt('paragraph_flow', 1, 62),
    ]
    const previousLayout = fakeLayout([true, true])
    const previousRendered = compactReplaceParagraphRenderSlots(previousSource, previousLayout)
    const currentSource = [slotAt('paragraph_flow', 0, 180, 'r1')]

    const shift = replaceParagraphContinuationShift({
      sourceSlots: previousSource,
      renderedSlots: previousRendered,
      layout: previousLayout,
      groupShift: 0,
      sourceText: 'the negative log likelihood still corresponds',
    }, currentSource, 'to the estimated coding bits.')

    const previousBottom = previousRendered[1].bounds.y + previousRendered[1].bounds.height
    const shiftedGap = currentSource[0].bounds.y + shift - previousBottom
    expect(shift).toBeLessThan(0)
    expect(shiftedGap).toBeCloseTo(34 * 0.65, 5)
  })

  it('does not pull a distant paragraph across a real section break', () => {
    const previousSource = [slotAt('paragraph_flow', 0, 20), slotAt('paragraph_flow', 1, 62)]
    const previousLayout = fakeLayout([true, false])
    const previousRendered = compactReplaceParagraphRenderSlots(previousSource, previousLayout)
    const currentSource = [slotAt('paragraph_flow', 0, 180, 'r1')]

    expect(replaceParagraphContinuationShift({
      sourceSlots: previousSource,
      renderedSlots: previousRendered,
      layout: previousLayout,
      groupShift: 0,
      sourceText: 'This is a complete sentence.',
    }, currentSource, 'A new paragraph starts here.')).toBe(0)
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
