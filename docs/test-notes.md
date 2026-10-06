# Test notes to create for the notedown repository

These notes replace the private Bike and Content notes and fill the gaps listed in
`docs/note-format.md` section 12. Everything in them becomes public: write only invented content
(no real names, addresses, passwords or work topics).

## General rules

- Device: Supernote A6 X2 (Nomad), stable firmware (currently Chauvet 3.29.42). Note the firmware
  version in the commit message when you add the files.
- File names: exactly as listed below (the device appends `.note`). They become
  `tests/fixtures/notes/<name>.note` in the repository.
- Template: "8mm ruled line" unless a note says otherwise.
- Note type: "Standard" unless a note says "Real-Time Recognition (RTR)". For RTR notes choose the
  language given, and close the note only after the recognized text has appeared.
- Pen: needle point, black, default width, unless a note says otherwise.
- Write clearly but naturally; a few crossed-out words are welcome (they show how OCR handles them).
- For every page, also export a PNG on the device (Export > PNG) and copy it next to the `.note` file.
  The PNGs become the device-export references.
- Do not edit a note again after it is finished, so the file holds one clean save history (except
  `nd-12-edits`, which is about editing).

## The notes

| # | Name | Type, language | Pages | Tests |
|---|---|---|---|---|
| 1 | `nd-01-text-en` | Standard | 1 | Plain English text |
| 2 | `nd-02-text-de` | Standard | 1 | German text, umlauts, compound words |
| 3 | `nd-03-rtr-de` | RTR, German | 1 | Device text recognition (RECOGNTEXT) |
| 4 | `nd-04-lists` | Standard | 1 | Bullets, numbering, checkboxes, nesting |
| 5 | `nd-05-tables` | Standard | 2 | Tables with and without lines, merged cells |
| 6 | `nd-06-diagrams` | Standard | 2 | Flowchart, sequence, mind map, boxes with arrows |
| 7 | `nd-07-math` | Standard | 1 | Formulas, fractions, exponents |
| 8 | `nd-08-headings` | Standard | 2 | Device titles (headings) in all levels |
| 9 | `nd-09-keywords-stars` | Standard | 2 | Keywords and the five-star mark |
| 10 | `nd-10-links-hub` | Standard | 2 | File, page and web links |
| 11 | `nd-11-links-target` | Standard | 2 | Target of the links in note 10 |
| 12 | `nd-12-edits` | Standard | 1 | Eraser, lasso move, edits after saving |
| 13 | `nd-13-pens` | Standard | 1 | Pen types, widths, gray levels, marker |
| 14 | `nd-14-layers` | Standard | 1 | Extra layers (Layer 1-3), a hidden layer |
| 15 | `nd-15-templates` | Standard | 4 | Different templates incl. blank, grid, custom image |
| 16 | `nd-16-landscape` | Standard | 1 | Landscape page |
| 17 | `nd-17-blank-pages` | Standard | 3 | Empty pages between written ones |
| 18 | `nd-18-textbox` | Standard | 1 | Typed text box (keyboard) next to handwriting |
| 19 | `nd-19-messy` | Standard | 1 | Fast, sloppy handwriting, mixed DE/EN |

## Details per note

### 1. `nd-01-text-en`
One page of plain English prose, about 6 to 8 lines in two paragraphs. Include a few numbers
(a date like 2026-10-12, a time like 14:30, an amount like 12.50 EUR) and one email-like word
(`hello@example.com`). Tests line joining, paragraphs and digits.

### 2. `nd-02-text-de`
One page of German prose with umlauts and ß in many words (Übersicht, Größe, Äpfel, Straße, müssen),
two long compound words (Fahrradreparaturwerkstatt, Wochenendeinkaufsliste) and one sentence with
quotation marks. Tests German OCR quality.

### 3. `nd-03-rtr-de`
Create as Real-Time Recognition note, language German (de_DE). Write 4 to 5 short German sentences.
Wait until the recognized text appears, then close the note. Tests that the parser reads the device's
own recognition text (we have no sample with stored text yet).

### 4. `nd-04-lists`
- A bullet list (3 items) with dots.
- A numbered list (1., 2., 3.).
- A list where item 2 has two sub-items (indented).
- A to-do list with drawn boxes: two empty boxes, one box with a check mark, one with a cross.
Tests list and checkbox conversion.

### 5. `nd-05-tables`
- Page 1: a table drawn with full lines, 3 columns x 4 rows, with a header row (for example
  "Item | Qty | Price"). Below it a second small table where one header cell spans two columns
  (a merged cell).
- Page 2: a table without any lines, just aligned columns (3 columns x 4 rows), like a timetable.
Tests table detection, Markdown tables, and HTML tables for merged cells.

### 6. `nd-06-diagrams`
- Page 1: a flowchart with 5 boxes: Start, a decision diamond with "yes"/"no" branches, two actions,
  End. Arrows with labels. Below it three boxes in a row with double-headed arrows between them.
- Page 2: a simple sequence diagram (two vertical lines "Client" and "Server", 3 labeled arrows
  between them) and a small mind map (one center word, 4 branches, one branch with 2 sub-branches).
Tests Mermaid output for different diagram kinds.

### 7. `nd-07-math`
Three to four formulas: `E = mc²`, a fraction like `(a + b) / 2`, a square root, and a sum with an
index (`Σ i = 1 .. n`). One formula on its own line, one inline inside a sentence.
Tests math conversion (`$...$` and `$$...$$`).

### 8. `nd-08-headings`
On page 1 write three lines and mark them as titles on the device with three different title
styles (the device offers several heading levels or styles; use as many different ones as it has).
Write some normal text under each. On page 2, one more title and text. Tests the device's title
blocks (we have no sample with titles).

### 9. `nd-09-keywords-stars`
Page 1: write a few lines, then add two keywords with the device's keyword function (for example
select a word with the lasso and make it a keyword, and add one typed keyword). Page 2: mark the page
with the five-star mark (the star on the page). Tests keywords and the star mark (no samples yet).

### 10. `nd-10-links-hub`
A small "table of contents" note.
- Page 1: three handwritten words, each made into a link:
  - "Target" as a file link to `nd-11-links-target`,
  - "Target page 2" as a link to page 2 of `nd-11-links-target`,
  - "Website" as a web link to `https://example.com`.
  Below them, one normal paragraph with one linked word inside the sentence (link it to
  `nd-11-links-target` as well).
- Page 2: a link back to page 1 of this note (a link inside the same note).
Tests all link kinds, the block-replacement rule and links inside a paragraph.

### 11. `nd-11-links-target`
Two pages with a title-like line on each ("Target page 1", "Target page 2") and some text. If the
device can add a link pointing back to `nd-10-links-hub`, add one on page 2. Tests link resolution
between converted notes.

### 12. `nd-12-edits`
Write 5 lines and close the note. Open it again, erase one word with the eraser, erase half of a
line with the eraser, move one word with the lasso to another place, and add one new line. Close it.
Tests erased and moved strokes and a file with several saves.

### 13. `nd-13-pens`
One line each with: needle point, ink pen (fountain pen), marker/highlighter over a word, calligraphy
pen if available. Use the different gray levels (black, dark gray, light gray) and at least two
different widths. Tests pen types, colors and stroke widths in the vector data.

### 14. `nd-14-layers`
Write a line on the main layer. Add Layer 1 and write a second line there, add Layer 2 and draw a
circle there, then hide Layer 2. Tests extra layers and layer visibility.

### 15. `nd-15-templates`
Four pages, each with a different template and one line of text:
1. blank (no template),
2. grid / squared,
3. dotted,
4. a custom template from an image (if the device allows a custom PNG template; otherwise any other
   built-in template).
Tests template handling and the PNG template path.

### 16. `nd-16-landscape`
Rotate the page to landscape and write 3 lines and a small sketch. Tests landscape pages (no sample
yet).

### 17. `nd-17-blank-pages`
Three pages: page 1 with two lines of text, page 2 completely empty, page 3 with two lines of text.
Tests that empty pages are detected and skipped by OCR.

### 18. `nd-18-textbox`
Add a text box with the keyboard and type one sentence. Next to it, write one sentence by hand.
Tests typed text boxes (PAGETEXTBOX) next to handwriting.

### 19. `nd-19-messy`
Write quickly and a bit sloppy, as in a real meeting: a mix of German and English, abbreviations,
an arrow between two words, an underlined word and a circled word. Tests the realistic worst case.

## After writing

Copy all `.note` files and their PNG exports into one folder and tell me where it is. I then:

1. add them to `tests/fixtures/notes/` and the PNGs to `tests/fixtures/device/`,
2. regenerate the golden renderings,
3. run the format checks and OCR on all of them and update `docs/note-format.md` with what the new
   files show (titles, keywords, stars, layers, landscape, templates, pens),
4. replace the tests that use the private Bike and Content notes.
