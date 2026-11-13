# Evaluating in other languages

Whether an answer is in the language it was asked in, written in the script its readers read,
right once Unicode is accounted for, and as good in Malay or Thai as in English, measured on your
own traffic. evald ships no benchmark data, because benchmark licences rarely allow it and your
traffic is the test that matters. The first targets are the languages of Malaysia, Singapore and
their neighbours: Malay, Indonesian, Thai, Vietnamese, Tamil and Chinese.

## What evald can tell, and what it cannot

Languages are detected by an embedded trigram detector, [whatlang](https://github.com/greyblake/whatlang-rs)
(MIT). It makes no network call, needs no model files, and covers 70 languages.

**Malay and Indonesian are one language here.** No detector we measured tells them apart. The
accuracy reports that ship with [lingua](https://github.com/pemistahl/lingua-rs) 1.8.0
(`accuracy-reports/`, read 2026-10-22) give these figures:

| Detector | Malay sentences right | Malay read as Indonesian | Indonesian sentences right |
|---|---|---|---|
| lingua, high accuracy | 28.1% | 68.9% | 82.7% |
| lingua, low accuracy | 34.3% | 58.1% | 71.2% |
| CLD2 | 23.1% | 66.4% | 88.1% |
| whatlang | — (no Malay) | — | 95.5% |

ISO 639 already has the honest grouping. `ms` is the Malay *macrolanguage*, and Indonesian (`id`)
and Standard Malay (`zsm`) are members of it. So evald labels a detected Malay or Indonesian text
`ms`, and that label satisfies an expected `ms`, `id`, `zsm` or `ms-MY` alike. The check catches
an answer in English to a question in Malay, which is the failure that matters. It does not claim
a distinction it cannot make. If you need to tell the two apart, put the market in the item's
`metadata` yourself and slice by it.

**Short Latin-script text is not placed.** whatlang's confidence is a poor guide for Latin-script
text. Correct Malay and English sentences score 0.01-0.5, and every wrong detection we measured
was shorter than 20 letters: "Thank you for waiting." was read as Norwegian, and "Paris" as Latin.
So evald acts on a detection when the detector is confident or the text has 20 letters or more.
Shorter text is skipped and counted, not guessed. A script that names its language (Thai, Tamil,
Han, kana) is placed at any length. The test `a_detection_is_acted_on_only_when_it_can_be_trusted`
in `src/lang.rs` holds the examples this rests on.

## The evaluators

```yaml
evaluators:
  - type: language_match          # the item's metadata.language, or `language: ms`
  - type: script                  # the language's script, or `script: Jawi` / `Thai` / `Han`
    min: 0.8
  - type: exact_match
    normalize: true               # NFKC + lower-case: ＫＬ２０２６ == kl2026
  - type: levenshtein
    normalize: true
```

- **`language_match`**: 1 when the answer is in the expected language. The comment names what was
  detected (`expected ms, detected en (English, confidence 0.54)`).
- **`script`**: the share of the answer's letters in the expected script. It catches a Thai answer
  in transliteration, or Malay in Rumi where Jawi was asked for. Digits and punctuation do not
  count, but a product or brand name in Latin letters does count against: `ราคา iPhone 15 คือ
  39,900 บาท` is 10 Thai letters out of 16 (0.63). A full answer that names it stays above the
  default 0.8; set `min` for how much Latin your answers carry.
- **`normalize: true`** on `exact_match` and `levenshtein` compares the two texts after NFKC, then
  lower-casing, then NFKC again. Full-width forms, ligatures, composed and decomposed Vietnamese
  diacritics, and case all stop failing a match. This is not full case folding: `ß` stays `ß`.

## Every report by language

Give the items a `metadata.language`. Every run then reports each evaluator per language, and
`eval compare --non-inferior` gives a verdict per language ([CONFIG.md](./CONFIG.md#non-inferiority---non-inferior---margin)):

```text
by language           evaluator               items    mean
   en                 language_match              1   1.000
   ms                 language_match              2   0.500
   th                 language_match              1   1.000
   en                 script                      1   1.000
   ms                 script                      2   1.000
   th                 script                      1   1.000
```

(Four answers, one of the two asked in Malay answered in English: `script` passes it, since
English and Malay share a script, and `language_match` does not.)

`slice_by:` in the config names other `metadata` keys to report by. `language`, `route` and `task`
are the default.

## A per-language set from your own traffic

```bash
# 1. Your production calls, each labelled with the language its prompt is in.
evald dataset from-spans --limit 5000 --detect-language --out traffic.jsonl

# 2. How much of each language there is.
jq -r '.metadata.language // "unplaced"' traffic.jsonl | sort | uniq -c | sort -rn

# 3. One set per language, each item with an id.
for l in ms th ta vi zh en; do
  jq -c --arg l "$l" 'select(.metadata.language == $l) | .id = .span_id' traffic.jsonl > "set-$l.jsonl"
done
```

4. **Have a speaker of each language read a sample.** The detector labels the prompt's language,
   which is not always the language the answer should be in: a Malay speaker may ask in English.
   Correct the labels the reader disagrees with.
5. **Add what to score against.** Either an `expected_output` for the questions that have one
   right answer, or a human score on the span (`POST /v1/span_annotations`) to calibrate a judge.
6. Run every config over the language sets. `language_match` and `script` need nothing more than
   the label.

## Judging in other languages

evald's built-in rails (`qa_correctness`, `faithfulness`, …) are English rubrics. A capable judge
model reads an answer in Malay against an English rubric well, and keeping one rubric keeps
scores comparable across languages. Some rubrics are clearer in the language of the answer: what
counts as polite, or as the right register. Do not assume either way. The choice is yours to
measure:

**An English rubric** (the default): the built-in rails, or a `g_eval` with English criteria
that names the language:

```yaml
judges:
  - name: judge_helpful_ms
    rail: g_eval
    endpoint: judge-box
    model: prometheus-7b
    criteria: >-
      The answer is in Malay. Score 1 if it answers the customer's question correctly and
      politely, in Malay, and 0 if it is wrong, impolite, or not in Malay.
```

**A translated rubric**: the same criteria written in the answer's language:

```yaml
  - name: judge_helpful_ms_translated
    rail: g_eval
    endpoint: judge-box
    model: prometheus-7b
    criteria: >-
      Beri skor 1 jika jawapan menjawab soalan pelanggan dengan betul dan sopan dalam bahasa
      Melayu, dan 0 jika jawapan salah, tidak sopan, atau bukan dalam bahasa Melayu.
```

**Calibrate each, per language.** Give each language's judge a name of its own, as above. Run it
on that language's labelled set, then compare it with the people who labelled it:

```bash
evald eval run --config judge-ms.yaml
evald eval calibrate --judge judge_helpful_ms --pass-threshold 0.5
evald eval calibrate --judge judge_helpful_ms_translated --pass-threshold 0.5
```

Keep the rubric whose kappa against your Malay labels is higher. A judge calibrated on English
labels says nothing about how it judges Malay ([JUDGES.md](./JUDGES.md#before-you-trust-it-calibrate-on-your-own-labels)).

## The language tax

A tokenizer that splits Thai into one token per character charges three or four times what
English costs for the same answer. `evald cost language-tax` measures it from your stored calls:
tokens per character of each model's answers, by language. It also gives each language's ratio to
the same model's English ([CONFIG.md](./CONFIG.md#evald-cost-language-tax)). Read it across models
within a language. Run `evald replay` of one dataset on each candidate model first, so that every
model answers the same questions ([MIGRATING.md](./MIGRATING.md)).
