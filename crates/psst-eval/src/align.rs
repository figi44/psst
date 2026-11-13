//! Word alignment: Levenshtein distance over words, with the path that achieves it.

/// How a hypothesis lines up with its reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alignment {
    /// Substitutions, deletions and insertions.
    pub errors: usize,
    /// For each hypothesis word: whether it's aligned to an equal reference word.
    pub matched: Vec<bool>,
}

/// Aligns `hypothesis` against `reference` with the fewest errors. Ties prefer a match or
/// substitution, then a deletion, so the result is deterministic.
pub fn align<T: PartialEq>(reference: &[T], hypothesis: &[T]) -> Alignment {
    let (n, m) = (reference.len(), hypothesis.len());
    let width = m + 1;
    let mut cost = vec![0usize; (n + 1) * width];
    for (j, cell) in cost[..width].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        cost[i * width] = i;
        for j in 1..=m {
            let diagonal = cost[(i - 1) * width + j - 1] + usize::from(reference[i - 1] != hypothesis[j - 1]);
            let deletion = cost[(i - 1) * width + j] + 1;
            let insertion = cost[i * width + j - 1] + 1;
            cost[i * width + j] = diagonal.min(deletion).min(insertion);
        }
    }

    let mut matched = vec![false; m];
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        let here = cost[i * width + j];
        if i > 0 && j > 0 {
            let same = reference[i - 1] == hypothesis[j - 1];
            if here == cost[(i - 1) * width + j - 1] + usize::from(!same) {
                matched[j - 1] = same;
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && here == cost[(i - 1) * width + j] + 1 {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    Alignment { errors: cost[n * width + m], matched }
}
