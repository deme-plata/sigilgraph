# SIGIL, ordklasse for ordklasse

*En grammatisk gennemgang af en kæde, hvor hver ordklasse får lov at bære præcis den del af
sandheden, den er bygget til at bære. Alle tal er målt på den kørende node den 28. august
2026 — ingen af dem er skøn.*

---

## Substantiverne — tingene, der findes

Dansk har et ord for alt, hvad man kan pege på, og en blockchain består udelukkende af den
slags. **Noten.** **Nullifieren.** **Ankeret.** **Beviset.** **Puljen.** Fem substantiver,
og du har hele SIGILs private lag.

En **note** er et beløb, der ligger i en pulje, låst bag en forpligtelse. Ingen kan se
beløbet. En **nullifier** er det mærke, du afslører, når du bruger noten — kæden husker
mærket, aldrig noten. Et **anker** er roden af det træ, du beviser medlemskab i. Et **bevis**
er den matematiske påstand om, at alt ovenstående hænger sammen, uden at røbe hvordan.

Læg mærke til, hvad der *ikke* er et substantiv her: en konto. Der findes ingen kontoer i
det skjulte lag. Der findes kun noter, og det er hele forskellen.

## Verberne — hvad kæden faktisk gør

Substantiver er nemme. Verberne er der, hvor systemerne dør.

At **udstede** er let. At **skjule** er svært. At **bevise** er svært på en anden måde. Og
det verbum, der væltede os denne uge, var det, ingen havde tænkt over: at **lægge sammen**.

SIGILs kredsløb tog nøjagtig én note ind. Én ind, to ud. Og deraf følger noget, folk bliver
overraskede over hver eneste gang:

> Det beløb, du kan sende i én transaktion, er ikke din saldo. Det er din største enkeltnote.

Man kan ikke lægge to noter sammen ved at sende til sig selv — det er stadig én ind og to
ud, så man ender med *flere* noter, aldrig færre. Sammenlægning var ikke uimplementeret. Med
ét input var det **aritmetisk umuligt**.

## Adjektiverne — påstandene, man skal passe på

Adjektiver er de farligste ord i kryptografi, fordi de lyder som egenskaber og opfører sig
som markedsføring. **Privat.** **Sikker.** **Endelig.** **Decentral.**

Så lad os bruge dem præcist.

v4-kredsløbet var **sundt** — det beviste nøjagtigt, hvad det påstod. Det var ikke
**skjulende**. Forskellen er ikke akademisk: v4 holdt hemmelighederne i konstante
spor-kolonner, så modtagerens nøgle og begge beløb lå i klartekst i beviset, cirka 85
forekomster hver. Beviset var korrekt. Beviset var også en opslagstavle.

Det er derfor, adjektiverne skal deles op. "Sundt" og "skjulende" er to forskellige
egenskaber, og et system kan have den ene uden den anden i lang tid, uden at nogen opdager
det.

## Adverbierne — hvor ærligheden bor

Her er den ordklasse, de fleste tekniske tekster udelader, og det er præcis derfor, de
fleste tekniske tekster lyver en lille smule.

**Endnu ikke.** **Næsten.** **Kun.** **Stadig.** **Målt.** **Ubekræftet.**

Et adverbium er det, der gør en sætning sand i stedet for imponerende. "SIGIL er privat" er
et adjektiv, der løber løbsk. "SIGIL beviser **nu** ejerskab **uden** at offentliggøre
vidnet, **men** kun i det kredsløb, der **faktisk** kaldes" — det er den samme påstand,
gjort sand af fire adverbier og en konjunktion.

Og det var netop et adverbium, der manglede i en hel dag: v5-rettelsen var skrevet, testet og
committet — og **aldrig kaldt**. Den lå som dødt kode, mens hver eneste rigtige transaktion
blev ved med at offentliggøre sit vidne. En rettelse, ingen kalder, er ikke en rettelse.

## Pronominerne — hvem, og hvorfor det er det svære spørgsmål

**Jeg. Du. Nogen. Ingen.**

Anonymitet handler ikke om beløb. Den handler om pronominer. Spørgsmålet er aldrig "hvor
meget", det er "**hvem**" — og et anonymitetssæt måles derfor i personer, ikke i noter.

Det lærte vi på den dyre måde. Puljen indeholdt 836.536 noter, og det lød som et
storslået anonymitetssæt. Men hver eneste mønt-note blev udstedt i den blok, dens ejer
selv havde mineret, og den blok nævner mineren. Chronos målte det direkte: **620 ud af 620**
noter kunne tilskrives deres ejer offentligt i udstedelsesøjeblikket.

836.536 noter. Ét pronomen. Det er ikke dækning, det er polstring.

## Numeralierne — de tal, der ikke er til forhandling

| | |
|---|---|
| Noter i puljen | **836.536** |
| Værdi låst i dem | **18.039 SIGIL** |
| Gennemsnitlig note | **0,0216 SIGIL** |
| Nullifiere afsløret nogensinde | **1** |
| Registrerede tegnebøger | **34** |
| Blokrate, målt over 90 sekunder | **2,66/sek** |

En pulje, der var skrevet til 836.536 gange og læst fra én gang. **77,5 %** af al SIGIL, der
nogensinde er udstedt, lå i stykker for små til at bruge.

Bemærk det sidste tal i tabellen: 2,66. Vi havde regnet med 6,28, fordi det tal stod i vores
egne noter. Det var en indhentningsrate, ikke en driftsrate — og et vindue udregnet på det
forkerte tal er 2,4 gange kortere, end det ser ud. Numeralier skal måles. Ikke huskes.

## Præpositionerne — forholdene, der bærer hele designet

Præpositioner er de små ord, ingen lægger mærke til, og de er hele arkitekturen.

Værdi ligger **i** puljen. Et bevis peger **mod** et anker. En note hører **til** en ejer.
Et beløb bevæger sig **fra** det gennemsigtige lag **ind i** det skjulte — og **ud** igen.
En nullifier bindes **til** en position **i** et træ.

Det sidste "i" er ikke pynt. Fordi nullifieren bindes til positionen *i træet*, betyder den
samme rå nullifier to forskellige noter i to forskellige generationer — og hvis man
behandler dem som ens, fryser man en ærlig brugers penge fast for altid.

Hele epoke-rotationen findes på grund af ét forholdsord.

## Konjunktionerne — logikken, hvor pengene faktisk tabes

**Og. Eller. Men. Fordi. Hvis.**

To indgange **og** to nullifiere. Det lyder trivielt. Det var det ikke.

Fodrer man ét kredsløb med *den samme note* som begge indgange, er begge blokke uafhængigt
gyldige, bevarelseslinjen starter på `værdi + værdi`, **og** alle begrænsninger holder.
Kredsløbet accepterer det. Det eneste tegn er, at de to nullifiere er ens — **men** kæden
gemmer nullifiere i en *mængde*, så den anden indsættelse er en tom handling.

Én note brændt. Dobbelt værdi ud.

Det er et helt system, der falder på forskellen mellem "og" og "og forskellige". Kontrollen
kan ikke ligge i kredsløbet, **fordi** "disse to vidner er forskellige" ikke er en påstand om
nogen enkelt række. Den skal ligge udenfor. Det gør den nu.

## Interjektionerne — den ordklasse, en logbog aldrig indeholder

**Av.**

Det er den ærlige reaktion på at opdage, at en rettelse, man var stolt af, aldrig blev kaldt.
Og på at en test, man selv skrev, får noden til at gå ned med `left: 46, right: 33`, fordi
`winterfell` bruger `assert_eq!` dér, hvor man havde regnet med en fejlkode — hvilket vil
sige, at enhver fremmed maskine kunne vælte en node med korrekt indpakket vrøvl.

Interjektioner står ikke i commit-beskeder. De burde måske. De markerer nøjagtigt de steder,
hvor man lærte noget, og en logbog uden dem lyder, som om alt gik efter planen.

---

## Kendeordene — det bestemte og det ubestemte

Til sidst den mindste ordklasse, og den, der afgør, om en sætning er sand.

**En** note er ikke **noten**. **En** løsning er ikke **løsningen**.

Vi har bygget **et** to-input-kredsløb, ikke **det** endelige. Det beviser og verificerer, og
tretten prøver bekræfter det — **men** kæden kan ikke bruge det endnu: transaktionsformatet
bærer kun én nullifier, og tilstandslaget skriver dem ikke atomart. Kredsløbet er færdigt.
Vejen ind i kæden er det ikke.

Og de 836.536 noter, der allerede ligger i puljen, er ikke **kryptografisk** strandede — enhver
enkelt ejer kan hente sit eget tilbage med gentagne sammenlægninger. Det, der ikke kan lade
sig gøre billigt, er at rydde puljen som helhed: én sammenlægning fjerner én note, så det er
omtrent 836.535 transaktioner, uanset hvordan man planlægger dem.

Forhindringen er økonomisk. Ikke matematisk.

Den forskel er et enkelt kendeord værd.
