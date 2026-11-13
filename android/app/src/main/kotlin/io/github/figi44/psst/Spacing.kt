package io.github.figi44.psst

/** [text] as it goes after [before], the text before the cursor: with a space if it needs one. */
fun spaced(before: CharSequence?, text: String) =
    if (!text.first().isWhitespace() && needsSpace(before)) " $text" else text

/**
 * Whether dictated text goes after [before] with a space: not at the start of the field, after
 * whitespace, or after an opening bracket or quote.
 */
private fun needsSpace(before: CharSequence?): Boolean {
    if (before.isNullOrEmpty()) return false
    val last = before.last()
    if (last.isWhitespace() || last in OPENERS) return false
    if (last == '"' || last == '\'') {
        // A straight quote opens when it starts a word.
        val previous = before.getOrNull(before.length - 2) ?: return false
        return !previous.isWhitespace() && previous !in OPENERS
    }
    return true
}

private const val OPENERS = "([{<“‘«‹„‚¿¡"
