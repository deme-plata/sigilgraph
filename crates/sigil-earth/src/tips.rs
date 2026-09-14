//! Fortolkning — one interpretation per metric, written once here and served with the data so
//! every page shows the same tip beside the same number. Each tip says three things: what the
//! number is, how to read it today, and what it is NOT (the honest half).

use serde_json::{json, Value};

pub fn tips() -> Value {
    json!({
        "k_resid": {
            "name": "K⊕ (de-wobbled)",
            "what": "How far today's rotation vector sits from where the known cycles said it would be, in residual σ: the pole (x_p, y_p) after trend + annual + semiannual + Chandler 433 d, and the day length after trend + annual + semiannual, combined in quadrature like a χ with three degrees of freedom.",
            "read": "Below 1: Earth is doing what the model expects. 1–3: one of the three is a bit off, usually the day length after a storm season or an ENSO shift. Above the p99 line: something the model does not know about happened — that is the alert.",
            "not": "It is not a forecast of earthquakes or weather, and it is not the raw pole offset. It is the surprise, not the size.", "da": "Hvor langt dagens rotationsvektor ligger fra det, de kendte cyklusser forudsagde, i residual-σ. Under 1: som forventet. Over p99-linjen: noget uforklaret er sket med Jordens ur.",
            "family": "Same shape as the chain's K_fix: a disagreement term (residual/σ) inside a square root. Here the three channels are geophysical, not consensus channels."
        },
        "k_raw": {
            "name": "K⊕ raw",
            "what": "The same three quantities scored against the plain mean and spread of the trailing year, with no cycles removed.",
            "read": "It sits above 1 most of the time because the Chandler and annual wobbles alone move the pole by metres. That is the point: it shows what the de-wobbling takes out.",
            "not": "Not the headline number. A high K_raw with a low K_resid means 'big but expected'.", "da": "Samme tre størrelser mod det seneste års middel og spredning, uden cyklusser fjernet. Høj K_raw og lav K_resid betyder \"stort, men forventet\"."
        },
        "ladder": {
            "name": "The ladder",
            "what": "Empirical quantiles of K⊕ over the trailing three years (p50 / p90 / p99 / max), beside the χ₃ values a Gaussian would give (1.54 / 2.50 / 3.37).",
            "read": "Compare today's K⊕ to the p99: above it, the alert fires; between p90 and p99, watch. If the empirical p99 sits well above 3.37, the residuals have fat tails and the model is missing a cycle.",
            "not": "Not a fixed scale. It moves as the window moves.", "da": "Empiriske fraktiler (p50/p90/p99) af K⊕ over tre år, ved siden af χ₃-værdierne for en gaussisk null. Over p99 udløses alarmen."
        },
        "xp": {"name": "x_p", "what": "Where the rotation axis pierces the crust, in arcseconds toward the Greenwich meridian (IERS convention). 1″ ≈ 30.9 m on the ground.", "read": "Follows a ~14-month spiral (Chandler wobble) mixed with an annual loop; the model_xp line is that spiral. The gap between the two is what K⊕ scores.", "not": "Not the geographic pole moving in any everyday sense: the whole excursion is a few metres.", "da": "Polens x-koordinat i buesekunder mod Greenwich-meridianen; 1″ ≈ 30,9 m på jordoverfladen. Afvigelsen fra modellinjen er det, K⊕ scorer."},
        "yp": {"name": "y_p", "what": "The other pole coordinate, toward 90° West (IERS convention). In the right-handed frame the page uses, ω_y = −Ω₀·y_p.", "read": "Read it with x_p as one point on the polhode. The dashed forecast path is the IERS prediction of where that point goes next.", "not": "Its sign is a convention; only the distance from the model matters for K⊕.", "da": "Polens y-koordinat mod 90° vest (IERS-konvention). Læses sammen med x_p som ét punkt på polbanen."},
        "lod": {"name": "LOD", "what": "Excess length of day over 86 400 s, in milliseconds. Positive: the day is longer than nominal, Earth spins slower.", "read": "Seasonal ±0.5 ms from the atmosphere, a 13.66 d and 27.55 d tidal ripple of ±0.5 ms, and slow decadal swings from the core. Today's value beside the fluid estimate tells you how much of it is weather.", "not": "It is not a trend: the secular tidal braking is ~2 ms per century, invisible at this scale.", "da": "Døgnets overskydende længde i millisekunder. Positiv: dagen er længere, Jorden roterer langsommere. Sæson ±0,5 ms fra atmosfæren, tidevandsrippel ±0,5 ms, langsomme svingninger fra kernen."},
        "ut1utc": {"name": "UT1−UTC", "what": "How far the Earth-angle clock (UT1) has drifted from atomic UTC, in seconds. Its slope is −LOD.", "read": "Approaching ±0.9 s forces a leap second. In 2020–2024 the day was often shorter than nominal, UT1−UTC climbed, and a negative leap second was discussed for the first time; a positive LOD like today's pulls it back the other way. The 3D globe is turned by this number.", "not": "Not a measurement of the day length directly; the day length is its derivative.", "da": "Jordens rotationsvinkel-ur (UT1) minus atomtid (UTC), i sekunder. Hældningen er −LOD. Kloden på 3D-siden drejes efter dette tal."},
        "omega": {"name": "ω vector", "what": "The rotation vector in rad/s: ω_x, ω_y from the pole offset, ω_z from the day length. Nominal Ω₀ = 7.292115e-5 rad/s.", "read": "ω_z − Ω₀ is of order 1e-13 rad/s — one part in a billion. The picture is exact; the deviation is tiny and real.", "not": "Not the axis of figure (the crust's symmetry axis): the small angle between the two is the polar motion.", "da": "Rotationsvektoren i rad/s: ω_x, ω_y fra polafvigelsen, ω_z fra døgnlængden. Afvigelsen fra Ω₀ er én milliardtedel — lille, men reel."},
        "pole_offset_m": {"name": "Pole offset", "what": "Distance on the crust between the instantaneous rotation pole and the reference pole, R⊕·√(x_p²+y_p²).", "read": "Typically 3–15 m, a spiral with a ~6.4-year beat between the annual and Chandler terms. On the globe it is exaggerated ×40 000 and labelled so.", "not": "Not a drift of the continents; the reference pole is fixed to the crust.", "da": "Afstanden på skorpen mellem den øjeblikkelige rotationspol og referencepolen, R⊕·√(x_p²+y_p²). Typisk 3–15 m. På kloden tegnet ×40 000 og mærket sådan."},
        "E_rot_J": {"name": "Rotational energy", "what": "½·I·ω² with I = 8.034e37 kg·m² (assumed constant).", "read": "About 2.1e29 J. A 1 ms longer day removes about 5e21 J from it — the dE per ms figure — which is roughly the world's annual energy use, moved into or out of the fluids and the core.", "not": "Not an energy the fluids 'consume'; it is exchanged as angular momentum.", "da": "Rotationsenergien ½·I·ω², ca. 2,1·10²⁹ J. Én ms længere dag flytter ca. 5·10²¹ J — omtrent verdens årlige energiforbrug."},
        "L_kg_m2_s": {"name": "Angular momentum", "what": "I·ω, about 5.9e33 kg·m²/s. Conserved for the Earth system: what the atmosphere gains, the solid Earth loses.", "read": "That conservation is why AAM predicts LOD: a westerly wind burst spins the air up and the ground down by the same amount.", "not": "Not conserved for the solid Earth alone.", "da": "Impulsmomentet I·ω. Bevaret for hele Jord-systemet: hvad atmosfæren vinder, taber den faste Jord. Derfor forudsiger AAM døgnlængden."},
        "equator_speed_m_s": {"name": "Equator speed", "what": "ω_z·R⊕, about 465 m/s.", "read": "Changes by ~5 µm/s per ms of LOD. Shown to give the vector a human scale.", "not": "Not a measurement; a derived number.", "da": "Ækvatorhastigheden ω_z·R⊕, ca. 465 m/s. Ændres med ~5 µm/s pr. ms LOD. Vist for at give vektoren en menneskelig skala."},
        "lod_atm_ms": {"name": "Atmosphere (AAM)", "what": "GFZ's effective angular momentum of the atmosphere (ECMWF winds + pressure) converted to a length-of-day change: 86 400 s × χ₃, in ms.", "read": "Carries most of the seasonal LOD signal: the northern winter jet streams spin the air up and the day lengthens.", "not": "A model product, not a measurement. It has no tides in it.", "da": "Atmosfærens effektive impulsmoment (ECMWF-vinde + tryk) omregnet til døgnlængde-ændring i ms. Bærer det meste af sæsonsignalet. Modelprodukt, ikke måling."},
        "lod_ocn_ms": {"name": "Ocean (OAM)", "what": "The same from the MPIOM ocean model (currents + bottom pressure).", "read": "Small but not negligible at the seasonal scale; strongest during ENSO events.", "not": "Model product.", "da": "Det samme fra havmodellen MPIOM (strømme + bundtryk). Lille, men ikke ubetydelig; stærkest under ENSO-hændelser."},
        "lod_hyd_ms": {"name": "Land water (HAM)", "what": "The same from the LSDM land-hydrology model (soil moisture, snow, rivers).", "read": "Slow and small in LOD; it matters more for the pole (χ₁, χ₂) than for the day length.", "not": "Model product.", "da": "Det samme fra landhydrologimodellen LSDM (jordfugt, sne, floder). Betyder mere for polen end for døgnlængden."},
        "lod_geo_ms": {"name": "Fluids, total", "what": "Atmosphere + ocean + land water, ms of LOD.", "read": "Put beside the observed LOD after the 31-day smoothing: R² 0.7 means seven tenths of the season-to-season day-length change IS the fluids. The rest is the core and model error.", "not": "It cannot explain the tidal ripple (the models do not carry tides), which is why the daily R² is low.", "da": "Atmosfære + hav + landvand i ms LOD. Efter 31-dages glidende middel forklarer væskerne ca. 7/10 af sæsonvariationen; resten er kernen og modelfejl."},
        "attribution_r2": {"name": "R² attribution", "what": "How much of the observed LOD variance the fluid estimate explains, daily and after a 31-day running mean.", "read": "Daily R² is dragged down by the zonal tides; the smoothed R² is the fair number. Both are reported so nobody has to trust the choice.", "not": "Not a fit of the fluids to LOD; the slope and offset only calibrate the models' subtracted mean.", "da": "Hvor stor en del af den observerede LOD-varians væskeestimatet forklarer, dagligt og efter 31-dages middel. Det daglige R² trækkes ned af tidevandet; det glattede er det fair tal."},
        "k_resid_fcst": {"name": "K⊕ forecast", "what": "The IERS-predicted pole and UT1−UTC for the next ten days, pushed through the same model and σ as today's K⊕. LOD on those days is the finite difference of the predicted UT1−UTC, because prediction rows carry no LOD.", "read": "Trust the first three days; after that the IERS pole prediction and the fluid forecast both lose skill. The band is the IERS-stated prediction error propagated.", "not": "It is not our own prediction of the pole; it is a reading of the IERS prediction through our model.", "da": "IERS-forudsagt pol og UT1−UTC ti dage frem gennem samme model og σ som i dag. LOD på de dage er afledt af UT1−UTC-differensen. Stol på de første tre dage."},
        "k_resid_fcst_gfz": {"name": "K⊕ forecast, fluid opinion", "what": "The same K⊕ with the LOD slot filled by the GFZ 10-day fluid forecast (calibrated), instead of the IERS UT1−UTC difference.", "read": "Two independent forecasts of the same number. When they agree, believe both; when they split, the gap is the tides plus the core.", "not": "Never averaged into the headline forecast.", "da": "Samme K⊕, men med GFZ's 10-dages væskeprognose i LOD-pladsen. To uafhængige prognoser af samme tal; når de er enige, tro på begge."},
        "eam90": {"name": "±90-day EAM prediction", "what": "GFZ's combined atmosphere + ocean + hydrology prediction three months out (Dill et al. 2018).", "read": "Useful for the direction of the seasonal swing, not its daily value.", "not": "Not a forecast of K⊕; it feeds only the LOD channel.", "da": "GFZ's kombinerede forudsigelse af atmosfære + hav + hydrologi tre måneder frem. Nyttig for sæsonsvingningens retning, ikke for den daglige værdi."},
        "era": {"name": "Earth Rotation Angle", "what": "The angle the globe has turned since the reference epoch, from your clock plus UT1−UTC: ERA = 2π(0.7790572732640 + 1.00273781191135448·(JD_UT1 − 2451545)).", "read": "The 3D globe is turned by exactly this, so Greenwich sits where it really is right now and the terminator is where the Sun really puts it.", "not": "Not sidereal time (that adds precession); the difference is under a degree.", "da": "Jordens rotationsvinkel siden referenceepoken, fra dit ur plus UT1−UTC. 3D-kloden drejes præcis dette, så Greenwich står, hvor det står lige nu."},
        "attest": {"name": "Attestation", "what": "BLAKE3 of the exact bytes of latest.json, chained to the previous digest and signed with a dedicated Ed25519 key. An anchor row records the shielded SIGIL transaction that carried the digest in its memo.", "read": "Anyone can recompute the digest from the published file and check the signature against the published key. The chain receipt proves the digest existed at that block; it does not prove the reading is true.", "not": "Not a proof of correctness; a proof of non-alteration with a timestamp nobody can rewrite.", "da": "BLAKE3 af de præcise bytes i latest.json, kædet til forrige digest og signeret med en dedikeret Ed25519-nøgle; en anker-række noterer SIGIL-transaktionen med digestet i memoet. Beviser ikke-ændring, ikke sandhed."},
        "alert": {"name": "Alert", "what": "K⊕ above the empirical p99 of the trailing three years (fires once, re-arms below p90), or a step on the K-family ladder.", "read": "One alert means: look at the fluids panel first. If the fluid estimate also jumped, it is weather; if not, it is the core or a data revision.", "not": "Not an earthquake or weather warning. IERS rapid values are revised for weeks; an alert can be revised away.", "da": "K⊕ over det empiriske p99 (udløses én gang, genaktiveres under p90) eller et trin på K-familiens stige. Én alarm betyder: se på væskepanelet først."}
    })
}

/// What the numbers say *today*: one computed sentence per metric, English and Danish, merged into
/// the static tips as `today` / `today_da`. This is the fortolkning of the actual reading, not of
/// the metric in general.
pub struct TodayCtx {
    pub date: String,
    pub k_resid: f64,
    pub k_raw: f64,
    pub regime: String,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub lod: f64,
    pub lod_atm: Option<f64>,
    pub lod_ocn: Option<f64>,
    pub lod_hyd: Option<f64>,
    pub lod_geo: Option<f64>,
    pub r2_daily: Option<f64>,
    pub r2_sm31: Option<f64>,
    pub pole_offset_m: f64,
    pub xp: f64,
    pub yp: f64,
    pub ut1utc: Option<f64>,
    pub omega_z_minus: f64,
    pub de_per_ms: f64,
    pub fc1_k: Option<f64>,
    pub fc1_k_gfz: Option<f64>,
    pub fc1_date: Option<String>,
    pub attest_n: Option<u64>,
    pub attest_anchored: bool,
    pub armed: bool,
}

pub fn with_today(mut tips: Value, c: &TodayCtx) -> Value {
    let f2 = |v: f64| format!("{v:.2}");
    let ms = |v: f64| format!("{}{:.3} ms", if v >= 0.0 { "+" } else { "" }, v);
    let band = if c.k_resid > c.p99 { "above the p99 line — the alert condition" } else if c.k_resid > c.p90 { "between p90 and p99 — watch" } else if c.k_resid > c.p50 { "above the median, inside the usual spread" } else { "below the median — a quiet day" };
    let band_da = if c.k_resid > c.p99 { "over p99-linjen — alarmbetingelsen" } else if c.k_resid > c.p90 { "mellem p90 og p99 — hold øje" } else if c.k_resid > c.p50 { "over medianen, inden for den sædvanlige spredning" } else { "under medianen — en rolig dag" };
    let mut set = |key: &str, en: String, da: String| {
        if let Some(t) = tips.get_mut(key) {
            t["today"] = json!(en);
            t["today_da"] = json!(da);
        }
    };
    set("k_resid",
        format!("{}: {}σ, {} — {} (p50 {}, p90 {}, p99 {}).", c.date, f2(c.k_resid), c.regime, band, f2(c.p50), f2(c.p90), f2(c.p99)),
        format!("{}: {}σ, {} — {} (p50 {}, p90 {}, p99 {}).", c.date, f2(c.k_resid), c.regime, band_da, f2(c.p50), f2(c.p90), f2(c.p99)));
    set("k_raw",
        format!("Raw {} against de-wobbled {}: the cycles {} of the apparent excursion today.", f2(c.k_raw), f2(c.k_resid), if c.k_raw > c.k_resid { "explain part" } else { "explain none" }),
        format!("Rå {} mod af-vaklet {}: cyklusserne forklarer {} af dagens tilsyneladende udsving.", f2(c.k_raw), f2(c.k_resid), if c.k_raw > c.k_resid { "en del" } else { "intet" }));
    set("ladder",
        format!("Today's reading sits {}. The alert is {}.", band, if c.armed { "armed" } else { "disarmed until K⊕ falls below p90" }),
        format!("Dagens aflæsning ligger {}. Alarmen er {}.", band_da, if c.armed { "aktiv" } else { "deaktiveret, til K⊕ falder under p90" }));
    let fluids = match (c.lod_geo, c.lod_atm) {
        (Some(g), Some(a)) => format!(" The fluids account for {} of it (atmosphere {}); the rest is tides and the core.", ms(g), ms(a)),
        _ => String::new(),
    };
    let fluids_da = match (c.lod_geo, c.lod_atm) {
        (Some(g), Some(a)) => format!(" Væskerne står for {} af det (atmosfæren {}); resten er tidevand og kernen.", ms(g), ms(a)),
        _ => String::new(),
    };
    set("lod",
        format!("{}: the day was {} {} than 86 400 s.{}", c.date, ms(c.lod.abs()).trim_start_matches('+'), if c.lod >= 0.0 { "longer" } else { "shorter" }, fluids),
        format!("{}: dagen var {} {} end 86 400 s.{}", c.date, ms(c.lod.abs()).trim_start_matches('+'), if c.lod >= 0.0 { "længere" } else { "kortere" }, fluids_da));
    if let (Some(a), Some(o), Some(h)) = (c.lod_atm, c.lod_ocn, c.lod_hyd) {
        set("lod_geo_ms", format!("Atmosphere {}, ocean {}, land water {} → fluids {}.", ms(a), ms(o), ms(h), ms(a + o + h)),
            format!("Atmosfære {}, hav {}, landvand {} → væsker {}.", ms(a), ms(o), ms(h), ms(a + o + h)));
        set("lod_atm_ms", format!("{} today — {}.", ms(a), if a.abs() > o.abs() + h.abs() { "the dominant fluid term, as usual" } else { "unusually, not the dominant term today" }),
            format!("{} i dag — {}.", ms(a), if a.abs() > o.abs() + h.abs() { "det dominerende væskeled, som sædvanligt" } else { "usædvanligt nok ikke det dominerende led i dag" }));
    }
    if let (Some(d), Some(s)) = (c.r2_daily, c.r2_sm31) {
        set("attribution_r2", format!("R² {} daily, {} after the 31-day mean: the fluids explain about {}% of the season-to-season day length this year.", f2(d), f2(s), (s * 100.0).round()),
            format!("R² {} dagligt, {} efter 31-dages middel: væskerne forklarer ca. {} % af sæsonvariationen i døgnlængden i år.", f2(d), f2(s), (s * 100.0).round()));
    }
    set("pole_offset_m", format!("{:.2} m from the reference pole (x_p {:.4}″, y_p {:.4}″) — {}.", c.pole_offset_m, c.xp, c.yp, if c.pole_offset_m > 10.0 { "the outer part of the Chandler-annual beat" } else { "the inner part of the beat" }),
        format!("{:.2} m fra referencepolen (x_p {:.4}″, y_p {:.4}″) — {}.", c.pole_offset_m, c.xp, c.yp, if c.pole_offset_m > 10.0 { "den ydre del af Chandler-års-svævningen" } else { "den indre del af svævningen" }));
    set("xp", format!("{:.6}″ toward Greenwich ≈ {:.1} m.", c.xp, c.xp * 30.9), format!("{:.6}″ mod Greenwich ≈ {:.1} m.", c.xp, c.xp * 30.9));
    set("yp", format!("{:.6}″ toward 90° W ≈ {:.1} m.", c.yp, c.yp * 30.9), format!("{:.6}″ mod 90° V ≈ {:.1} m.", c.yp, c.yp * 30.9));
    if let Some(u) = c.ut1utc {
        set("ut1utc", format!("{:+.4} s: Earth's clock is {} atomic time by {:.0} ms and the gap {} at {:.2} ms/day.", u, if u < 0.0 { "behind" } else { "ahead of" }, u.abs() * 1000.0, if (u < 0.0) == (c.lod > 0.0) { "widens" } else { "narrows" }, c.lod.abs()),
            format!("{:+.4} s: Jordens ur er {} atomtiden med {:.0} ms, og gabet {} med {:.2} ms/dag.", u, if u < 0.0 { "bagud for" } else { "foran" }, u.abs() * 1000.0, if (u < 0.0) == (c.lod > 0.0) { "vokser" } else { "krymper" }, c.lod.abs()));
    }
    set("omega", format!("ω_z − Ω₀ = {:.3e} rad/s: {} parts in 10¹³ {} nominal.", c.omega_z_minus, (c.omega_z_minus.abs() / 1e-13).round(), if c.omega_z_minus < 0.0 { "below" } else { "above" }),
        format!("ω_z − Ω₀ = {:.3e} rad/s: {} dele i 10¹³ {} det nominelle.", c.omega_z_minus, (c.omega_z_minus.abs() / 1e-13).round(), if c.omega_z_minus < 0.0 { "under" } else { "over" }));
    set("E_rot_J", format!("Today's {} of day length is {:.2e} J {} the rotational store.", ms(c.lod), c.de_per_ms * c.lod.abs(), if c.lod >= 0.0 { "taken out of" } else { "put back into" }),
        format!("Dagens {} døgnlængde er {:.2e} J {} rotationslageret.", ms(c.lod), c.de_per_ms * c.lod.abs(), if c.lod >= 0.0 { "taget ud af" } else { "lagt tilbage i" }));
    if let (Some(k), Some(d)) = (c.fc1_k, c.fc1_date.as_ref()) {
        let g = c.fc1_k_gfz.map(|v| format!(" The fluid opinion says {}.", f2(v))).unwrap_or_default();
        let g_da = c.fc1_k_gfz.map(|v| format!(" Væskeprognosen siger {}.", f2(v))).unwrap_or_default();
        set("k_resid_fcst", format!("For {}: {} through the IERS prediction — {}.{}", d, f2(k), crate::regime(k), g),
            format!("For {}: {} via IERS-forudsigelsen — {}.{}", d, f2(k), crate::regime(k), g_da));
    }
    if let Some(n) = c.attest_n {
        set("attest", format!("This publication becomes attestation row #{}; the previous row #{} is {}.", n + 1, n, if c.attest_anchored { "anchored on the SIGIL chain" } else { "signed, its on-chain anchor pending" }),
            format!("Denne udgivelse bliver attesteringsrække #{}; den forrige række #{} er {}.", n + 1, n, if c.attest_anchored { "forankret på SIGIL-kæden" } else { "signeret, kæde-ankeret afventer" }));
    }
    tips
}
