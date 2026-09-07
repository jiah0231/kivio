import type { LensReplaceGroup, LensReplaceRenderSlot } from '../api/tauri'
import {
  layoutReplaceTextFlow,
  normalizeReplaceParagraph,
  replaceTextVerticalOffset,
  type ReplaceTextFlowLayout,
  type ReplaceTextFlowSlotLayout,
} from './replaceTextLayout'

const FONT_FAMILY = 'system-ui, "Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei", sans-serif'

function fontSpec(fontPx: number) {
  return `${fontPx}px ${FONT_FAMILY}`
}

function median(values: number[]): number | undefined {
  const sorted = values
    .filter(value => Number.isFinite(value) && value > 0)
    .sort((left, right) => left - right)
  if (sorted.length === 0) return undefined
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2
}

type InkMetrics = {
  ascent: number
  descent: number
  height: number
  left: number
  measured: boolean
}

function inkMetrics(
  ctx: CanvasRenderingContext2D,
  text: string,
  fontPx: number,
): InkMetrics {
  ctx.font = fontSpec(fontPx)
  const metrics = ctx.measureText(text || '国Ag')
  const ascent = metrics.actualBoundingBoxAscent
  const descent = metrics.actualBoundingBoxDescent
  const left = metrics.actualBoundingBoxLeft
  if (
    Number.isFinite(ascent)
    && Number.isFinite(descent)
    && ascent + descent > 0
  ) {
    return {
      ascent,
      descent,
      height: ascent + descent,
      left: Number.isFinite(left) ? left : 0,
      measured: true,
    }
  }
  // Width-only Canvas mocks and older WebViews do not expose ink metrics. Keep
  // the legacy top-baseline path in that case instead of inventing geometry.
  return { ascent: 0, descent: 0, height: fontPx, left: 0, measured: false }
}

/**
 * Rust sends `sourceFontPx` from the source glyph ink extent. Canvas `font:px`
 * is an em-box size, not an ink height. Treating one as the other made every
 * replacement smaller. Calibrate the target system font so its measured ink
 * height matches the source ink height.
 */
export function calibrateReplaceFontPx(
  ctx: CanvasRenderingContext2D,
  sourceInkPx: number,
  sample: string,
): number {
  const targetInk = Math.max(1, sourceInkPx || 16)
  const probePx = 100
  const probe = inkMetrics(ctx, sample || '国Ag', probePx)
  if (!probe.measured || probe.height <= 0) return targetInk
  return Math.max(1, probePx * targetInk / probe.height)
}

export function replaceSlotContentBox(slot: LensReplaceRenderSlot) {
  const edgeInset = 1
  const left = slot.align === 'left'
    ? Math.max(slot.bounds.x, slot.anchor.x)
    : slot.bounds.x + edgeInset
  const right = slot.bounds.x + slot.bounds.width - edgeInset
  return {
    width: Math.max(1, right - left),
    height: Math.max(1, slot.bounds.height - edgeInset * 2),
    maxLines: slot.flow === 'paragraph_flow' || slot.flow === 'exact_line' ? 1 : undefined,
  }
}

function groupText(group: LensReplaceGroup, slots: LensReplaceRenderSlot[]) {
  const raw = group.translated.trim() || group.sourceText
  if (!raw) return ''
  return slots.some(slot => slot.flow === 'paragraph_flow')
    ? normalizeReplaceParagraph(raw)
    : raw.replace(/\r\n?/g, '\n')
}

/**
 * Paragraph OCR boxes contain the source line's available vertical geometry.
 * The backend's sourceFontPx is intentionally conservative and may be ~20-30%
 * below that geometry; for CJK translations that makes a short translation use
 * far fewer source baselines, leaving conspicuous blank bands before the next
 * paragraph fragment. Let paragraph text grow toward the actual line box while
 * keeping a hard cap at 1.6x the reported ink and at 92% of the slot height.
 * Standalone labels/headings continue to use the reported source ink exactly.
 */
export function replaceGroupSourceInkPx(slots: LensReplaceRenderSlot[]): number {
  const reported = median(slots.map(slot => slot.sourceFontPx)) ?? 16
  const paragraphHeights = slots
    .filter(slot => slot.flow === 'paragraph_flow')
    .map(slot => slot.bounds.height)
  if (paragraphHeights.length === 0) return reported

  const lineBox = median(paragraphHeights)
  if (!lineBox) return reported
  const geometryTarget = lineBox * 0.92
  return Math.max(reported, Math.min(geometryTarget, reported * 1.6))
}

function shiftedSlot(slot: LensReplaceRenderSlot, dy: number): LensReplaceRenderSlot {
  if (Math.abs(dy) < 0.001) return slot
  return {
    ...slot,
    bounds: { ...slot.bounds, y: slot.bounds.y + dy },
    anchor: {
      ...slot.anchor,
      y: slot.anchor.y + dy,
      baselineY: slot.anchor.baselineY + dy,
    },
  }
}

function isParagraphGroup(slots: LensReplaceRenderSlot[]) {
  return slots.length > 0 && slots.every(slot => slot.flow === 'paragraph_flow')
}

/**
 * OCR line boxes can contain a single very large y jump (math superscript,
 * merged display formula, detector jitter). Rendering at those absolute y
 * anchors tears one translated sentence into two pieces with a giant blank
 * stripe in the middle. For a semantic paragraph the source baselines are only
 * a typography hint, so collapse outlier gaps to the paragraph's typical pitch.
 * The first baseline stays fixed and each following used baseline advances by
 * a robust median pitch capped by the source line-box height.
 */
export function compactReplaceParagraphRenderSlots(
  slots: LensReplaceRenderSlot[],
  layout: ReplaceTextFlowLayout,
  groupShift = 0,
): LensReplaceRenderSlot[] {
  if (!isParagraphGroup(slots)) {
    return slots.map(slot => shiftedSlot(slot, groupShift))
  }

  const usedIndices = slots
    .map((_, index) => (layout.slots[index]?.lines.length ?? 0) > 0 ? index : -1)
    .filter(index => index >= 0)
  if (usedIndices.length === 0) {
    return slots.map(slot => shiftedSlot(slot, groupShift))
  }

  const lineBox = median(slots.map(slot => slot.bounds.height)) ?? 1
  const sourceDeltas = slots
    .slice(1)
    .map((slot, index) => slot.anchor.y - slots[index].anchor.y)
    .filter(delta => Number.isFinite(delta) && delta > 0)
  const sourcePitch = median(sourceDeltas) ?? lineBox * 1.15
  const pitch = Math.max(lineBox * 0.92, Math.min(sourcePitch, lineBox * 1.55))
  const firstIndex = usedIndices[0]
  const firstAnchorY = slots[firstIndex].anchor.y + groupShift
  let usedOrdinal = 0

  return slots.map((slot, index) => {
    if ((layout.slots[index]?.lines.length ?? 0) === 0) {
      return shiftedSlot(slot, groupShift)
    }
    const targetAnchorY = firstAnchorY + usedOrdinal * pitch
    usedOrdinal += 1
    return shiftedSlot(slot, targetAnchorY - slot.anchor.y)
  })
}

function horizontalOverlapRatio(a: LensReplaceRenderSlot, b: LensReplaceRenderSlot) {
  const left = Math.max(a.bounds.x, b.bounds.x)
  const right = Math.min(a.bounds.x + a.bounds.width, b.bounds.x + b.bounds.width)
  return Math.max(0, right - left) / Math.max(1, Math.min(a.bounds.width, b.bounds.width))
}

function sourceBounds(slots: LensReplaceRenderSlot[]) {
  return {
    top: Math.min(...slots.map(slot => slot.bounds.y)),
    bottom: Math.max(...slots.map(slot => slot.bounds.y + slot.bounds.height)),
  }
}

function usedRenderedBottom(slots: LensReplaceRenderSlot[], layout: ReplaceTextFlowLayout) {
  const bottoms = slots
    .map((slot, index) => (layout.slots[index]?.lines.length ?? 0) > 0
      ? slot.bounds.y + slot.bounds.height
      : Number.NEGATIVE_INFINITY)
    .filter(value => Number.isFinite(value))
  return bottoms.length > 0 ? Math.max(...bottoms) : Number.NEGATIVE_INFINITY
}

type PreviousParagraphPlan = {
  sourceSlots: LensReplaceRenderSlot[]
  renderedSlots: LensReplaceRenderSlot[]
  layout: ReplaceTextFlowLayout
  groupShift: number
}

/**
 * Chinese translations are often shorter than English. If a paragraph uses
 * only the first N of M source rows, keeping the next paragraph at its original
 * absolute y leaves M-N empty rows on screen. Preserve the *original paragraph
 * gap* but remove the unused tail rows. This is intentionally limited to the
 * same text column and to nearby body blocks so menus/tables never get reflowed.
 */
export function replaceParagraphContinuationShift(
  previous: PreviousParagraphPlan,
  currentSlots: LensReplaceRenderSlot[],
): number {
  if (!isParagraphGroup(previous.sourceSlots) || currentSlots.length === 0) return 0

  const currentIsParagraph = isParagraphGroup(currentSlots)
  const currentIsWideLine = currentSlots.length === 1
    && currentSlots[0].flow === 'exact_line'
    && currentSlots[0].kind === 'line'
    && currentSlots[0].bounds.width >= (median(previous.sourceSlots.map(slot => slot.bounds.width)) ?? 1) * 0.5
  if (!currentIsParagraph && !currentIsWideLine) return 0

  const previousLast = previous.sourceSlots[previous.sourceSlots.length - 1]
  const currentFirst = currentSlots[0]
  const lineBox = median([
    ...previous.sourceSlots.map(slot => slot.bounds.height),
    ...currentSlots.map(slot => slot.bounds.height),
  ]) ?? 1
  const rawGap = sourceBounds(currentSlots).top - sourceBounds(previous.sourceSlots).bottom
  if (rawGap < -lineBox * 0.35 || rawGap > lineBox * 1.6) return 0
  if (Math.abs(previousLast.anchor.x - currentFirst.anchor.x) > lineBox * 1.8) return 0
  if (horizontalOverlapRatio(previousLast, currentFirst) < 0.55) return 0

  const renderedBottom = usedRenderedBottom(previous.renderedSlots, previous.layout)
  if (!Number.isFinite(renderedBottom)) return previous.groupShift
  const shiftedSourceBottom = sourceBounds(previous.sourceSlots).bottom + previous.groupShift
  const unusedTail = Math.max(0, shiftedSourceBottom - renderedBottom)
  return previous.groupShift - unusedTail
}

function contentBlockHeight(
  ctx: CanvasRenderingContext2D,
  lines: string[],
  fontPx: number,
  lineHeight: number,
) {
  if (lines.length === 0) return 0
  const lastMetrics = inkMetrics(ctx, lines[lines.length - 1], fontPx)
  return Math.max(lastMetrics.height, (lines.length - 1) * lineHeight + lastMetrics.height)
}

function firstLineTop(
  ctx: CanvasRenderingContext2D,
  slot: LensReplaceRenderSlot,
  slotLayout: ReplaceTextFlowSlotLayout,
  fontPx: number,
  lineHeight: number,
) {
  if (slot.flow === 'exact_line' || slot.verticalAlign === 'top') return slot.anchor.y
  const blockHeight = contentBlockHeight(ctx, slotLayout.lines, fontPx, lineHeight)
  return slot.bounds.y + replaceTextVerticalOffset(slot.kind, slot.bounds.height, blockHeight)
}

function drawSlotText(
  ctx: CanvasRenderingContext2D,
  slot: LensReplaceRenderSlot,
  slotLayout: ReplaceTextFlowSlotLayout,
  layout: ReplaceTextFlowLayout,
) {
  ctx.font = fontSpec(layout.fontPx)
  ctx.fillStyle = slot.sourceColor
  ctx.textAlign = slot.align

  let top = firstLineTop(ctx, slot, slotLayout, layout.fontPx, layout.lineHeight)
  for (const line of slotLayout.lines) {
    const metrics = inkMetrics(ctx, line, layout.fontPx)
    let x: number
    if (slot.align === 'left') {
      // Canvas left alignment is the typographic origin, not necessarily the
      // first ink pixel. Correct the side bearing so the translated ink begins
      // at the same x anchor as the source ink.
      x = slot.anchor.x + (metrics.measured ? metrics.left : 0)
    } else if (slot.align === 'center') {
      x = slot.bounds.x + slot.bounds.width / 2
    } else {
      x = slot.bounds.x + slot.bounds.width - 1
    }

    if (metrics.measured) {
      ctx.textBaseline = 'alphabetic'
      ctx.fillText(line, x, top + metrics.ascent)
    } else {
      // Preserve the existing mock/legacy behavior when ink metrics are not
      // available. This path also keeps older WebView2 installations usable.
      ctx.textBaseline = 'top'
      ctx.fillText(line, x, top)
    }
    top += layout.lineHeight
  }
}

function drawScaledSlotText(
  ctx: CanvasRenderingContext2D,
  slot: LensReplaceRenderSlot,
  slotLayout: ReplaceTextFlowSlotLayout,
  layout: ReplaceTextFlowLayout,
) {
  const scale = layout.safeScale
  const offscreen = document.createElement('canvas')
  offscreen.width = Math.max(1, Math.ceil(slot.bounds.width / scale))
  offscreen.height = Math.max(1, Math.ceil(slot.bounds.height / scale))
  const offscreenCtx = offscreen.getContext('2d')
  if (!offscreenCtx) return

  const virtualSlot: LensReplaceRenderSlot = {
    ...slot,
    bounds: { x: 0, y: 0, width: offscreen.width, height: offscreen.height },
    anchor: {
      x: (slot.anchor.x - slot.bounds.x) / scale,
      y: (slot.anchor.y - slot.bounds.y) / scale,
      baselineY: (slot.anchor.baselineY - slot.bounds.y) / scale,
    },
  }
  drawSlotText(offscreenCtx, virtualSlot, slotLayout, layout)
  ctx.drawImage(offscreen, slot.bounds.x, slot.bounds.y, slot.bounds.width, slot.bounds.height)
}

/** Draw all translated groups onto the already-cleaned captured image. */
export function renderReplaceTextGroups(
  ctx: CanvasRenderingContext2D,
  groups: LensReplaceGroup[],
  slots: LensReplaceRenderSlot[],
) {
  const slotsByGroup = new Map<string, LensReplaceRenderSlot[]>()
  for (const slot of slots) {
    const groupSlots = slotsByGroup.get(slot.groupId) ?? []
    groupSlots.push(slot)
    slotsByGroup.set(slot.groupId, groupSlots)
  }

  let previousParagraph: PreviousParagraphPlan | null = null

  for (const group of groups) {
    const groupSlots = [...(slotsByGroup.get(group.id) ?? [])]
      .sort((left, right) => left.anchor.y - right.anchor.y || left.anchor.x - right.anchor.x)
    if (groupSlots.length === 0) continue

    const text = groupText(group, groupSlots)
    if (!text) continue
    const sourceInkPx = replaceGroupSourceInkPx(groupSlots)
    const fontPx = calibrateReplaceFontPx(ctx, sourceInkPx, text.slice(0, 96))
    const layout = layoutReplaceTextFlow(
      text,
      groupSlots.map(replaceSlotContentBox),
      fontPx,
      (value, size) => {
        ctx.font = fontSpec(size)
        return ctx.measureText(value).width
      },
    )

    const groupShift = previousParagraph
      ? replaceParagraphContinuationShift(previousParagraph, groupSlots)
      : 0
    const renderSlots = compactReplaceParagraphRenderSlots(groupSlots, layout, groupShift)

    renderSlots.forEach((slot, index) => {
      const slotLayout = layout.slots[index]
      if (!slotLayout || slotLayout.lines.length === 0) return
      ctx.save()
      ctx.beginPath()
      ctx.rect(slot.bounds.x, slot.bounds.y, slot.bounds.width, slot.bounds.height)
      ctx.clip()
      if (layout.safeScale < 1) drawScaledSlotText(ctx, slot, slotLayout, layout)
      else drawSlotText(ctx, slot, slotLayout, layout)
      ctx.restore()
    })

    if (isParagraphGroup(groupSlots)) {
      previousParagraph = {
        sourceSlots: groupSlots,
        renderedSlots: renderSlots,
        layout,
        groupShift,
      }
    } else if (!(previousParagraph && groupShift !== 0)) {
      previousParagraph = null
    }
  }
}
