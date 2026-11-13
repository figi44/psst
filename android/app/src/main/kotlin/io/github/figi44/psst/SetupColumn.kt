package io.github.figi44.psst

import android.content.Context
import android.util.AttributeSet
import android.widget.LinearLayout

/** The setup screen's column: the full width of a phone, at most [MAX_WIDTH_DP] on anything wider. */
class SetupColumn(context: Context, attrs: AttributeSet?) : LinearLayout(context, attrs) {
    private val maxWidth = (MAX_WIDTH_DP * resources.displayMetrics.density).toInt()

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val width = MeasureSpec.getSize(widthMeasureSpec)
        val spec = if (width > maxWidth) MeasureSpec.makeMeasureSpec(maxWidth, MeasureSpec.EXACTLY) else widthMeasureSpec
        super.onMeasure(spec, heightMeasureSpec)
    }

    private companion object {
        const val MAX_WIDTH_DP = 600
    }
}
