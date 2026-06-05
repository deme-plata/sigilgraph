//! sigil-book — "Shadows in the Chain" book generator, SIGIL-native.
//!
//! Ported from Quillon Graph's `shadowchain-writer` (Server Beta), whose book
//! wrapper (`shadows-complete-improved.tex`) hand-assembled the LaTeX and
//! `\input{..}`-ed chapter files from the working directory. This version
//! builds the document through the **`flux-arxiv-latex` AST** (`Document` +
//! `Block`) and `include_str!`-embeds the *improved* chapter `.tex` files, so
//! the binary is fully self-contained.
//!
//! It reproduces the original wrapper faithfully (`book`, 12pt/letterpaper, the
//! neon cyberpunk preamble + titlepage) and adds the pandoc compatibility shims
//! (`\tightlist`, `\passthrough`) the chapter content needs to compile.
//!
//! Chapter 6 ("The Sigil Veil") is new — the SIGIL-secrecy chapter, written for
//! this network: provenance-signed binaries, four committed state roots, 10 ms
//! tip-verification, and the zero-knowledge mixer.
//!
//! Usage:
//!   sigil-book                 # render + compile PDF into ./book-out/
//!   sigil-book --tex-only      # render the .tex, skip PDF compilation
//!   sigil-book --out <dir>     # output directory (default: book-out)

use flux_arxiv_latex::{Block, Document};

/// Part dividers, keyed by the chapter number they precede. The book's arc
/// splits cleanly into three movements: the dead network, the living one, and
/// the forest of many. Emitted as a `\part{..}` before the matching chapter.
const PARTS: &[(usize, &str, &str)] = &[
    (1, "Part One: The Veil",
     "What you hide in a crowd can be found. What you trust can be betrayed."),
    (6, "Part Two: The Sigil",
     "Verify before you trust --- and build a world where you can."),
    (16, "Part Three: The Forest",
     "No system should be the last. A truth is only safe when many can check it."),
];

/// (embedded improved chapter content, "N: Title"). Each `.tex` carries its own
/// `\section{Chapter N: ...}` heading, so we emit it verbatim as `Block::Raw`.
const CHAPTERS: &[(&str, &str)] = &[
    (include_str!("../assets/chapter1_content_improved.tex"), "1: Digital Shadows"),
    (include_str!("../assets/chapter2_content_improved.tex"), "2: The Architect's Game"),
    (include_str!("../assets/chapter3_content_improved.tex"), "3: Quantum Entanglement"),
    (include_str!("../assets/chapter4_content_improved.tex"), "4: The Singapore Protocol"),
    (include_str!("../assets/chapter5_content_improved.tex"), "5: The Moscow Gambit"),
    (include_str!("../assets/chapter6_content_improved.tex"), "6: The Sigil Veil"),
    (include_str!("../assets/chapter7_content_improved.tex"), "7: The Builders' Quorum"),
    (include_str!("../assets/chapter8_content_improved.tex"), "8: The Twenty-One Million"),
    (include_str!("../assets/chapter9_content_improved.tex"), "9: The Agents' Witness"),
    (include_str!("../assets/chapter10_content_improved.tex"), "10: Reproducible Minds"),
    (include_str!("../assets/chapter11_content_improved.tex"), "11: The Bookseller's Node"),
    (include_str!("../assets/chapter12_content_improved.tex"), "12: The Everything Merchant"),
    (include_str!("../assets/chapter13_content_improved.tex"), "13: The Sync That Lied"),
    (include_str!("../assets/chapter14_content_improved.tex"), "14: The Onion and the Eye"),
    (include_str!("../assets/chapter15_content_improved.tex"), "15: The University of No One"),
    (include_str!("../assets/chapter16_content_improved.tex"), "16: The Bridge of Two Truths"),
    (include_str!("../assets/chapter17_content_improved.tex"), "17: The Cost of Truth"),
    (include_str!("../assets/chapter18_content_improved.tex"), "18: The Night the Network Rewrote Itself"),
    (include_str!("../assets/chapter19_content_improved.tex"), "19: The Price of a Promise"),
    (include_str!("../assets/chapter20_content_improved.tex"), "20: The Unhurried Clock"),
    (include_str!("../assets/chapter21_content_improved.tex"), "21: The Day the Keys Fell"),
    (include_str!("../assets/chapter22_content_improved.tex"), "22: The Half the World"),
    (include_str!("../assets/chapter23_content_improved.tex"), "23: The Wages of Machines"),
    (include_str!("../assets/chapter24_content_improved.tex"), "24: The Pool of Many Hands"),
    (include_str!("../assets/chapter25_content_improved.tex"), "25: The Light in the Pocket"),
    (include_str!("../assets/chapter26_content_improved.tex"), "26: The First Block of the Rest"),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tex_only = args.iter().any(|a| a == "--tex-only");
    let out_dir = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "book-out".to_string());
    let job = "shadows_in_the_chain_improved";

    println!("📚 sigil-book — \"Shadows in the Chain\" (IMPROVED + Sigil Veil)");
    println!("   engine: flux-arxiv-latex AST → render → compile_pdf");

    let doc = build_document();

    let tex = doc.render();
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let tex_path = format!("{}/{}.tex", out_dir, job);
    std::fs::write(&tex_path, &tex).expect("write tex");
    println!("✓ Wrote {} ({} bytes)", tex_path, tex.len());

    if tex_only {
        println!("ℹ --tex-only: skipping PDF compilation.");
        return;
    }

    println!("🔨 Compiling PDF (tectonic → pdflatex fallback)...");
    let result = doc.compile_pdf(&out_dir, job);
    if result.success {
        println!(
            "✅ PDF compiled: {}",
            result.pdf_path.unwrap_or_else(|| format!("{}/{}.pdf", out_dir, job))
        );
    } else {
        eprintln!("⚠ PDF compilation failed (the .tex above is still valid).");
        eprintln!("--- engine log (tail) ---");
        for line in result.log.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
            eprintln!("{}", line);
        }
        std::process::exit(1);
    }
}

/// Assemble the `book` document, faithful to `shadows-complete-improved.tex`.
fn build_document() -> Document {
    // Verbatim preamble (after packages, before \begin{document}). Matches the
    // original wrapper's neon scheme + titleformat + hypersetup + fancyhdr, and
    // adds the pandoc shims the chapter `.tex` content requires.
    let preamble = r#"% pandoc compatibility shims (chapter content is pandoc-generated LaTeX)
\providecommand{\tightlist}{\setlength{\itemsep}{0pt}\setlength{\parskip}{0pt}}
\providecommand{\passthrough}[1]{#1}

% Cyberpunk noir color scheme
\definecolor{neonblue}{RGB}{0,255,255}
\definecolor{neongreen}{RGB}{57,255,20}
\definecolor{neonpink}{RGB}{255,16,240}
\definecolor{darkbg}{RGB}{10,10,15}

% Custom styling
\titleformat{\part}[display]
  {\normalfont\centering\color{neonpink}}
  {\Large\scshape Part \thepart}{18pt}{\Huge\bfseries\color{neonblue}}

\titleformat{\chapter}[display]
  {\normalfont\huge\bfseries\color{neonblue}}
  {\chaptertitlename\ \thechapter}{20pt}{\Huge}

\titleformat{\section}
  {\normalfont\Large\bfseries\color{neongreen}}
  {\thesection}{1em}{}

\titleformat{\subsection}
  {\normalfont\large\bfseries\color{neonpink}}
  {\thesubsection}{1em}{}

% Hyperlink styling
\hypersetup{
    colorlinks=true,
    linkcolor=neonblue,
    filecolor=neonpink,
    urlcolor=neongreen,
    pdftitle={Shadows in the Chain - Complete Manuscript},
    pdfauthor={AI-Generated Fiction},
    pdfsubject={Cyberpunk Thriller},
    pdfkeywords={quantum computing, cryptography, thriller, cyberpunk, sigil}
}

% Header/footer
\pagestyle{fancy}
\fancyhf{}
\fancyhead[LE,RO]{\thepage}
\fancyhead[LO]{\textit{Shadows in the Chain}}
\fancyhead[RE]{\textit{\leftmark}}
\renewcommand{\headrulewidth}{0.4pt}"#;

    // Title page + TOC, reproduced from the original wrapper (Chapters 1--6).
    let frontmatter = r#"\begin{titlepage}
    \centering
    \vspace*{2cm}

    {\Huge\bfseries\color{neonblue} SHADOWS IN THE CHAIN\par}
    \vspace{1cm}
    {\Large\itshape A Quantum Conspiracy Thriller\par}
    \vspace{2cm}

    {\large\color{neongreen}
    A Novel of Post-Quantum Cryptography,\\
    Algorithmic Governance,\\
    and the Price of Freedom\\
    \par}

    \vfill

    {\large\color{neonpink} Chapters 1--26\par}
    {\large Expert-Improved Edition --- the Sigil Cycle\par}

    \vfill

    {\large \today\par}
\end{titlepage}

% Opening epigraph page --- the book's thesis in three lines.
\thispagestyle{empty}
\vspace*{0.32\textheight}
\begin{center}
{\Large\itshape\color{neongreen} Verify before you trust.\par}
\vspace{0.5em}
{\itshape And then, having verified ---\par}
{\itshape trust, and keep watching.\par}
\vspace{2em}
{\small\color{neonpink}---\ the only law of the Sigil}
\end{center}
\newpage

\tableofcontents
\newpage"#;

    let mut doc = Document::new("book")
        .option("12pt")
        .option("letterpaper")
        .package_opt("inputenc", &["utf8"])
        .package_opt("fontenc", &["T1"])
        .package_opt("geometry", &["margin=1in"])
        .package("hyperref")
        .package("graphicx")
        .package("xcolor")
        .package("fancyhdr")
        .package("titlesec")
        .package("tcolorbox")
        .preamble(preamble)
        .add(Block::Raw(frontmatter.to_string()));

    for (idx, (content, title)) in CHAPTERS.iter().enumerate() {
        let chapter_no = idx + 1;
        if let Some((_, part_title, part_blurb)) = PARTS.iter().find(|(n, _, _)| *n == chapter_no) {
            println!("§ {}", part_title);
            // \part with a centred epigraph blurb beneath the heading.
            doc = doc.add(Block::Raw(format!(
                "\n\\part{{{}}}\n\\begin{{center}}\\itshape\\color{{neongreen}} {} \\end{{center}}\n\\vspace{{1em}}",
                part_title, part_blurb
            )));
        }
        println!("✓ Including chapter {}", title);
        doc = doc.add(Block::Raw(format!("\n% Chapter {}\n{}", title, content)));
    }

    doc
}
