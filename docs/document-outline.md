# Document Outline

Raven provides a hierarchical document outline view that displays symbols such as functions, variables, classes, sections, and loops across R, JAGS, and Stan files. This feature helps you navigate large files and understand code structure at a glance.

## Overview

The document outline appears in VS Code's **Outline** view (usually in the Explorer sidebar) and provides:

- **Hierarchical symbol tree** - See nested functions, variables within sections, and class methods
- **R code sections** - Organize your code with collapsible section headers (`# Section ----`)
- **R Markdown / Quarto headings** - Prose Markdown headings (`# Title`, `## Section`) form the document outline
- **R Markdown / Quarto chunks** - Each ```` ```{r ...} ```` block (and `# %%` cell in plain `.R` files) appears as its own outline entry, distinct from section headers
- **Rich symbol types** - Distinguish functions, constants, classes, and methods with distinct icons
- **Quick navigation** - Click any symbol to jump to its definition
- **JAGS/Stan model structure** - Navigate JAGS and Stan model files with blocks, decorative comment headings, and `for` loops
- **Breadcrumb navigation** - See your current position in the file structure

The outline updates automatically as you edit your code.

## Accessing the Outline

**VS Code:**
- Open the **Outline** view in the Explorer sidebar
- Use **Ctrl+Shift+O** (Windows/Linux) or **Cmd+Shift+O** (Mac) to open the symbol picker
- Enable breadcrumbs: **View → Show Breadcrumbs**

**Workspace symbols (project-wide search):**
- Use **Ctrl+T** (Windows/Linux) or **Cmd+T** (Mac) to search symbols across all files

## R Code Sections

Raven recognizes R code section comments as collapsible outline nodes. Sections help organize large files into logical groups.

### Section Syntax

A valid section comment requires:
1. One or more `#` characters (determines heading level)
2. A section name (text content)
3. Four or more delimiter characters: `-`, `=`, `#`, `*`, or `+`

```r
# Section Name ----
# Section Name ====
# Section Name ####
# Section Name ****
# Section Name ++++
```

**Optional features:**
- `%%` prefix for special sections: `# %% Section Name ----`
- Leading/trailing whitespace is ignored

### Heading Levels

Use multiple `#` characters to create nested sections:

```r
# Level 1 Section ----

## Level 2 Subsection ----

### Level 3 Subsection ----

# Another Level 1 Section ----
```

In the outline view:
- Level 1 sections (`#`) appear as top-level nodes
- Level 2 sections (`##`) nest under the preceding Level 1 section
- Level 3 sections (`###`) nest under the preceding Level 2 section

### Section Ranges

A section's range extends from its comment line to the line before the next sibling or parent section:

```r
# Data Loading ----
data <- read.csv("data.csv")

## Cleaning ----
data <- clean_data(data)

## Validation ----
validate_data(data)
# This line is still in the "Validation" section

# Analysis ----
# This starts a new Level 1 section
```

Symbols defined within a section's range appear as children of that section in the outline.

### Invalid Sections

These patterns are **not** recognized as sections:

```r
# ==================  (no text content - just delimiters)
# ----                (no text content)
# Section --          (only 2 delimiters, need 4+)
```

## R Markdown / Quarto Chunks

For `.Rmd` and `.qmd` files, every fenced code chunk gets its own outline entry. The label, if present, becomes the entry name; unlabeled chunks fall back to `Chunk #N` numbered in source order across the whole document. For non-R chunks (`{python}`, `{julia}`, ...) the language tag appears in the detail field.

````rmd
```{r setup, include=FALSE}
library(dplyr)
```

```{r}
analysis(data)
```

```{python}
print("hi")
```
````

Outline view shows:

```text
setup
Chunk #2
Chunk #3        {python}
```

Chunks use a distinct symbol kind (`OBJECT`) so the outline filter can include or exclude them separately from section headers.

R symbols defined *inside* an R chunk body — functions, variable assignments, S4/R6 classes — also appear in the outline, nested under their chunk, at their real document line. Prose, YAML front matter, and non-R chunk bodies are ignored, so a `library(...)` call in prose or a Python chunk never produces a phantom outline entry. (For non-R chunks only the chunk entry itself is shown.)

## R Markdown / Quarto Headings

For `.Rmd` and `.qmd` files, prose Markdown headings (`#`, `##`, …) form the document outline, so the Outline view and breadcrumbs reflect the document's section structure as you edit the source.

```rmd
---
title: "My Report"
---

# Introduction

Some prose.

```{r setup}
library(dplyr)
```

## Methods

```{r model}
fit <- lm(y ~ x, data = df)
```
```

Outline view shows:

```text
▼ Introduction
  ▼ setup
  ▼ Methods
    ▼ model
        fit
```

Details:

- **Levels nest by depth.** `#` is level 1, `##` is level 2, and so on through `######`; deeper headings nest under shallower ones, just like R code sections.
- **Chunks and chunk-body symbols nest under their heading.** A chunk appears under the heading whose section contains it, and the R symbols defined in that chunk nest under the chunk.
- **Only prose headings count.** Headings inside YAML front matter, fenced code blocks, and R chunk bodies are ignored, so a `#` comment in code never becomes a phantom heading. A `# Section ----` divider inside a chunk body still appears as an entry, but it does not compete with Markdown headings for the document's section structure.
- **Distinct icon.** Headings use the heading (string) symbol kind, so the outline filter can show or hide them separately from code chunks (object) and R sections (module).
- **ATX headings only.** Setext (underline-style) headings — text underlined with `===` or `---` — are not recognized; use `#`-prefixed (ATX) headings.

### `# %%` cells in `.R` files

Plain `.R` files use the VS Code interactive-cell convention: a `# %%` line starts a new cell that runs until the next marker, a section divider, or end of file. Any text after the marker is used as the label:

```r
# %% Setup
library(dplyr)

# %% Analysis
fit_model(data)
```

Cells with no trailing text fall back to `Chunk #N`.

> **Note:** A line like `# %% Setup ----` matches *both* the `# %%` cell-marker pattern and the section-divider pattern, so in a `.R` file it appears **twice** in the outline — once as a collapsible section (Module) and once as a cell entry (Chunk). Use `# %% Setup` (no trailing `----`) if you want only a plain cell.

## Symbol Types

Raven recognizes the following R constructs and assigns appropriate symbol types:

### Functions

```r
# Standard function - shows parameter list in detail
calculate <- function(x, y, z = 10) {
  x + y + z
}
```

**Icon:** Function symbol  
**Detail:** `(x, y, z = 10)` (parameter list)

### Variables

```r
# Regular variable assignment (RHS is a function call → Field)
data <- read.csv("data.csv")
result = process_data(data)
cached_value <<- compute_default()
```

**Icon:** Field symbol (LSP `SymbolKind::FIELD`)

This Field fallback applies only when the right-hand side isn't a recognized literal value. When the RHS *is* a literal, Raven assigns a more specific kind — see [Value-typed variables](#value-typed-variables) below.

### Value-typed variables

When a non-constant-named variable is assigned a literal value, Raven classifies it by the value's type rather than as a plain Field:

```r
flag    <- TRUE          # Boolean
count   <- 42            # Number (integer, float, or complex literal)
label   <- "summary"     # String
empty   <- NULL          # Null
missing <- NA            # Constant (also NA_integer_/…, Inf, NaN)
items   <- c(1, 2, 3)    # Array (c(), vector(), matrix(), array())
config  <- list(a = 1)   # List
```

Classification order is: class constructor (`R6Class`/`setRefClass`) → ALL_CAPS constant name → function definition → value type (above) → Field. So `MAX <- 42` is a Constant (name wins), while `count <- 42` is a Number.

### Constants

Variables with ALL_CAPS names (minimum 2 characters) are classified as constants:

```r
PI <- 3.14159
MAX_ITERATIONS <- 1000
DB_CONNECTION_STRING <- "postgresql://..."
```

**Icon:** Constant symbol  
**Pattern:** `^[A-Z][A-Z0-9_.]+$`

### Classes

```r
# R6 class
MyClass <- R6Class("MyClass",
  public = list(
    initialize = function(x) { self$x <- x }
  )
)

# S4 class
setClass("Person",
  slots = c(name = "character", age = "numeric")
)

# Reference class
MyRefClass <- setRefClass("MyRefClass",
  fields = list(value = "numeric")
)
```

**Icon:** Class symbol

### Methods

```r
# S4 method
setMethod("show", "Person",
  function(object) {
    cat("Person:", object@name, "\n")
  }
)
```

**Icon:** Method symbol  
**Name:** Extracted from the first string argument (`"show"`)

### Generics

```r
# S4 generic
setGeneric("process",
  function(x, ...) standardGeneric("process")
)
```

**Icon:** Interface symbol  
**Name:** Extracted from the first string argument (`"process"`)

## Hierarchical Nesting

The outline shows symbols nested within their containing scopes:

```r
# Data Processing ----

process_data <- function(df) {
  # These assignments appear as children of process_data()
  clean_df <- remove_na(df)
  validated_df <- validate(clean_df)
  
  # Nested function also appears as a child
  helper <- function(x) { x * 2 }
  
  return(validated_df)
}

# This variable is a child of the "Data Processing" section
CACHE_SIZE <- 1000
```

**Outline structure:**
```
▼ Data Processing
  ▼ process_data (df)
      clean_df
      validated_df
      helper (x)
  CACHE_SIZE
```

## JAGS and Stan Model Structure

Raven recognizes top-level block structures in JAGS and Stan files and displays them as the top-level hierarchy in the document outline. Decorative comment headings nest beneath those blocks, and `for` loops appear as intermediate outline containers for symbols declared inside them.

### JAGS Blocks

JAGS files (`.jags`, `.bugs`, `.bug`, case-insensitively) support two block types. These suffixes select strict JAGS behavior and do not imply general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility:

| Block | Outline Name |
|-------|--------------|
| `data { }` | data |
| `model { }` | model |

### Stan Blocks

Stan files (`.stan`) support seven block types:

| Block | Outline Name |
|-------|--------------|
| `functions { }` | functions |
| `data { }` | data |
| `transformed data { }` | transformed data |
| `parameters { }` | parameters |
| `transformed parameters { }` | transformed parameters |
| `model { }` | model |
| `generated quantities { }` | generated quantities |

Constrained declarations keep the declared identifier in the outline, so `real<lower=0, upper=1> foo;` appears as `foo`, not the constraint names.

### Decorative Comment Headings

JAGS and Stan files recognize **decorative headings only**. Plain title comments such as `// DIMENSIONS` or `# levels:` are not added to the outline.

Recognized patterns include banner fences:

```stan
// =====================================================================
// DIMENSIONS
// =====================================================================
```

and inline decorative headings:

```stan
// --- Incomplete official statistics (minima) ---
```

```jags
# --- Priors ---
```

Banner headings default to top-level within their enclosing block, while inline decorative headings default to the next level down. Existing hash-decorated banner title lines such as `## NAME ##` in JAGS preserve their explicit depth.

### Loops

`for` loops in JAGS and Stan files appear in the outline as container nodes. This applies to both braced loops and brace-less loops:

```stan
for (p_idx in 1:n_p1) {
  real foo = p_idx;
}

for (j in 1:M)
  real bar = j;
```

Symbols declared inside those loops appear as children of the corresponding loop node, and nested loops appear recursively in the outline.

### Example

```stan
data {
  // =====================================================================
  // DIMENSIONS
  // =====================================================================
  int<lower=0> N;
  vector[N] y;
}
parameters {
  real mu;
  real<lower=0> sigma;
}
model {
  // --- Likelihood ---
  for (n in 1:N) {
    y[n] ~ normal(mu, sigma);
  }
}
```

**Outline view shows:**
```text
▼ data
  ▼ DIMENSIONS
      N
      y
▼ parameters
    mu
    sigma
▼ model
  ▼ Likelihood
    ▼ for (n in 1:N)
```

Blocks are detected using text-based pattern matching with brace-depth tracking, so nested braces within block bodies are handled correctly. Decorative headings nest under their containing block instead of competing with block nodes at the top level. If a closing brace is missing, the block range extends to the end of the file.

## Workspace Symbol Search

Press **Ctrl+T** (Windows/Linux) or **Cmd+T** (Mac) to search for symbols across your entire project:

```
Type: "calc"
Results:
  calculate        main.R
  calculate_total  utils.R
  recalculate      analysis.R
```

**Features:**
- Substring matching (case-insensitive): "calc" matches "calculate"
- Shows file location for each symbol
- Jump to definition with Enter
- Searches open documents and indexed workspace files

## Configuration

### Workspace Symbol Limit

Control the maximum number of results returned by workspace symbol search:

```json
{
  "raven.symbols.workspaceMaxResults": 1000
}
```

| Setting | Default | Range | Description |
|---------|---------|-------|-------------|
| `raven.symbols.workspaceMaxResults` | 1000 | 100-10000 | Maximum symbols in Ctrl+T search results |

Higher values may impact performance on large projects.

## Examples

### Organizing a Large Analysis Script

```r
# Setup ----

library(dplyr)
library(ggplot2)

DATA_PATH <- "data/raw"
OUTPUT_PATH <- "output"

## Configuration ----

MAX_RECORDS <- 10000
CONFIDENCE_THRESHOLD <- 0.95

# Data Loading ----

load_data <- function(path) {
  read.csv(path, stringsAsFactors = FALSE)
}

raw_data <- load_data(DATA_PATH)

# Analysis ----

## Preprocessing ----

clean_data <- function(df) {
  df %>%
    filter(!is.na(value)) %>%
    filter(value > 0)
}

processed_data <- clean_data(raw_data)

## Modeling ----

fit_model <- function(df) {
  lm(y ~ x, data = df)
}

model <- fit_model(processed_data)
```

**Outline view shows:**
```
▼ Setup
    DATA_PATH
    OUTPUT_PATH
  ▼ Configuration
      MAX_RECORDS
      CONFIDENCE_THRESHOLD
▼ Data Loading
    load_data (path)
    raw_data
▼ Analysis
  ▼ Preprocessing
      clean_data (df)
      processed_data
  ▼ Modeling
      fit_model (df)
      model
```

## Troubleshooting

### Outline view is empty

1. Verify the file extension is `.R`, `.Rmd`, `.qmd`, `.jags`, `.bugs`, `.bug`, or `.stan`
2. Check that Raven is running (status bar shows "Raven")
3. Reload VS Code: **Ctrl+Shift+P** → "Developer: Reload Window"

### Sections not appearing in outline

Verify your section comment syntax:
- Needs at least 4 delimiter characters: `# Section ----` ✓
- Only 2-3 delimiters won't work: `# Section --` ✗
- Must have text content: `# ----` ✗

### Symbols showing at wrong level

The hierarchy is determined by:
1. Code position (line numbers)
2. Section heading levels (`#` vs `##` vs `###`)
3. Function body boundaries

Check that your section heading levels match your intended hierarchy.

### Workspace symbol search returns too few results

Increase the limit:
```json
{
  "raven.symbols.workspaceMaxResults": 5000
}
```

### Wrong symbol type/icon

Symbol types are determined in this order (first match wins):
- **Classes:** `R6Class()`, `setClass()`, `setRefClass()` calls
- **Constants:** ALL_CAPS naming pattern (2+ characters)
- **Functions:** `function(...)` on right-hand side
- **Value type:** literal right-hand side → Boolean (`TRUE`/`FALSE`), Number (numeric literal), String, Null (`NULL`), Constant (`NA`/`Inf`/`NaN`), Array (`c()`/`vector()`/`matrix()`/`array()`), or List (`list()`)
- **Variables:** all other assignments (Field)

So a literal assignment like `x <- 42` shows as a Number, not a Field — see [Value-typed variables](#value-typed-variables). A name like `MAX <- 42` is a Constant because the ALL_CAPS rule is checked before the value type.

Reserved words (if, else, for, while, TRUE, FALSE, NULL, etc.) are automatically filtered out.
