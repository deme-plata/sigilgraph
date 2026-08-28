# Til Sebastian — hvor SIGIL står, og hvorfor jeg bygger to kæder

*Viktor, 28. august 2026. Alle tal i denne tekst er målt på den kørende node samme dag. Dér
hvor noget er et skøn, står der, at det er et skøn.*

---

## Hvorfor jeg skriver nu i stedet for om en måned

Jeg overvejede at vente, til børnesygdommene var væk, så det hele fremstod pænere. Så tænkte
jeg på, hvad du selv ville sige til en elev, der afleverede en rapport uden usikkerheder.

Et resultat uden fejlkilder er ikke et pænere resultat. Det er et mindre troværdigt et.

Så denne tekst handler i vid udstrækning om, hvad der gik galt i denne uge, hvad vi målte, og
hvor vi tog fejl. Ikke fordi jeg er ydmyg, men fordi det er den eneste del, der faktisk
beviser noget om, hvordan vi arbejder. En kæde, der kan fortælle dig, hvordan den fejler — med
tal — er længere fremme end en, der ikke har opdaget det endnu.

## Den ene sætning, du forstår med det samme

Du kender UTXO-modellen. Så her er hele ugens problem i én sætning:

> **En note i SIGIL er en UTXO. Og indtil i onsdags kunne en transaktion kun bruge ÉN.**

Forestil dig en Bitcoin-tegnebog, der kun kan bruge ét input per transaktion. Du har 11 BTC —
men fordelt på 500 UTXO'er. Du kan sende 0,02. Ikke fordi der er en fejl, men fordi der ikke
findes en transaktion, der kan gøre andet. Og du kan ikke konsolidere dig ud af det: en
sammenlægning ville jo også kun have ét input.

Det er ikke en manglende feature. Det er aritmetik:

```
1 input, 2 outputs
⇒ største beløb ud ≤ største enkeltnote ind
⇒ ingen sekvens af transaktioner gør en note større
```

Det var SIGILs tilstand. Og en møntudstedelse skabte én note per miner per blok — ved den
målte blokrate omkring 230.000 nye støvnoter i døgnet.

## Hvad instrumentet viste, da vi endelig kiggede

| | |
|---|---|
| Noter i den skjulte pulje | **836.536** |
| Værdi låst i dem | **18.039 SIGIL** |
| Gennemsnitlig note | **0,0216 SIGIL** |
| Nullifiere afsløret nogensinde | **1** |
| Registrerede tegnebøger | **34** |

En pulje, der var skrevet til 836.536 gange og læst fra **én** gang. **77,5 %** af al SIGIL,
der nogensinde er udstedt, lå i stykker for små til at bruge.

En bruger rapporterede det fra sin telefon som *"jeg kan sende 0,006 ud af 11 SIGIL"*.
Han aflæste instrumentet fuldstændig korrekt. Det var tallet, der ikke betød det, det så ud
til at betyde — hvilket for mig at se er den værste slags fejl et system kan have. Ikke en
fejlmeddelelse. En rigtig værdi, der inviterer til en forkert aflæsning.

Og her er den detalje, der gjorde mig mest flov: en miner, der **ikke** havde slået privatliv
til, fik en almindelig saldo, der lægger sammen, og kunne bruge alt. En miner, der **havde**
slået det til, fik støv. Det var altså at vælge privatliv, der ødelagde muligheden for at
bruge sine penge.

## Tre børnesygdomme, og hvorfor de er det bedste i rapporten

### 1. En rettelse, der var færdig, testet, committet — og aldrig kaldt

Vi fandt, at det gamle bevis-kredsløb lækkede sit eget vidne: modtagerens nøgle og begge
beløb lå i klartekst i beviset, cirka 85 forekomster hver. Beviset var *korrekt* — det beviste
nøjagtigt, hvad det påstod — det **skjulte** bare ingenting. (Sundt og skjulende er to
forskellige egenskaber. Et system kan have det første uden det andet i lang tid.)

Vi skrev et nyt kredsløb, `v5`, der reserverer den anden halvdel af sporet til tilfældighed.
1160 linjer. Tests grønne. Committet.

Og så kaldte ingenting det. Tegnebogen blev ved med at bevise med det gamle kredsløb, og
verifikationen blev ved med at acceptere det. Rettelsen lå som dødt kode i et døgn, mens hver
eneste rigtige transaktion fortsatte med at offentliggøre sit vidne.

**En rettelse, ingen kalder, er ikke en rettelse.** Det er nu koblet på, og der ligger en test,
der ikke nævner noget kredsløb ved navn — den scanner simpelthen de bytes, tegnebogen er ved
at sende, for beløbene og nøglerne. Skifter nogen tilbage til det gamle kredsløb, fejler den
med det samme.

### 2. Et kredsløb, der accepterer, at man bruger den samme mønt to gange

Da vi byggede to-input-kredsløbet, bad jeg en gennemgang af designet. Den fandt noget, jeg
ikke havde set.

Giv kredsløbet **den samme note som begge indgange.** Begge blokke er uafhængigt gyldige — det
er jo det samme korrekte vidne to gange. Bevarelseslinjen starter på `værdi + værdi`. Alle
begrænsninger holder. Kredsløbet siger ja.

Det eneste spor er, at de to nullifiere bliver ens — og fordi kæden gemmer nullifiere i en
*mængde*, er den anden indsættelse en tom handling. Én note brændt, dobbelt værdi ud.

Vi skrev en test, der påstår, at kredsløbet accepterer det. Testen **består**. Den ligger der
netop for at fastholde, at kontrollen ikke kan ligge i kredsløbet — for "disse to vidner er
forskellige" er ikke en påstand om nogen enkelt række i sporet. Den skal ligge udenfor.

Havde vi koblet v6 på kæden uden den kontrol, havde vi bygget en pengemaskine.

### 3. En test, der væltede noden — og dermed fandt et angreb

Vi skrev en test af, at et v5-bevis og et v6-bevis aldrig kan forveksles. Den fejlede med
`left: 46, right: 33`.

Årsagen viste sig at være, at bevis-biblioteket bruger en `assert` — ikke en fejlkode — når
det får et bevis af forkert form. En `assert` **standser processen**. Og den funktion kaldes
på transaktioner, der kommer ind fra netværket.

Med andre ord: enhver fremmed maskine kunne vælte en node ved at sende en transaktion, der
påstår to nullifiere, med et bevis af den forkerte form vedhæftet. Der skulle ikke et gyldigt
bevis til. Kun korrekt indpakket vrøvl.

Det står der nu en kontrol foran. Men pointen er metoden: **fejlen blev fundet af en test, der
handlede om noget andet.** Det er den slags fund, man kun får, hvis man skriver tests, der
prøver at forveksle tingene, i stedet for tests, der bekræfter, at det hele virker.

## Om målinger, siden det er dit fag

To ting fra denne uge, som du vil kunne bruge direkte i en time.

**Vi regnede med den forkerte blokrate.** Vores egne noter sagde 6,28 blokke/sekund. Målt
direkte over 90 sekunder: **2,66**. De 6,28 var en *indhentningsrate* — hastigheden, når en
node haler ind på kæden — ikke driftsraten. To forskellige størrelser med samme enhed.

Konsekvensen var ikke akademisk: vi brugte tallet til at beregne, hvor lang tid operatører
havde til at opdatere før en regelændring. Det vindue var 2,4 gange kortere, end det så ud.

**Og vi troede, puljen var et stort anonymitetssæt.** 836.536 noter lyder overvældende. Men
hver mønt-note blev udstedt i den blok, dens ejer selv havde mineret — og blokken nævner
mineren. En simulering målte det direkte: **620 ud af 620** noter kunne tilskrives deres ejer
offentligt i udstedelsesøjeblikket.

Anonymitet måles i *personer*, ikke i noter. 836.536 sporbare støvnoter er ikke dækning. Det
er polstring. Og den erkendelse vendte hele beslutningen om: at udbetale minedrift
**gennemsigtigt** koster ikke noget privatliv, for der var ingen, og det fjerner samtidig hele
støvproblemet.

## Hvorfor to kæder — og hvorfor det ikke er splittet fokus

Det spørgsmål ville jeg selv stille. Her er svaret, og det hviler på noget, jeg først opdagede
i dag.

Quillon Graph har et kendt problem: nye noder er ikke enige med de gamle om tilstanden. Vi har
sporet det til noget dybere end en fejl — **DEX, lån, vault og staking kan slet ikke
rekonstrueres fra kædens historik.** Det er ikke en bug, det er et arkitekturhul. Tilstanden
findes, men den er ikke udledt af kæden, så en ny node kan ikke regne sig frem til den.

Og så er der det her sammenfald, som jeg ikke havde forudset:

> I Quillon findes `balance_smt.rs` — 1086 linjer, 15 tests, et komplet Sparse Merkle Tree
> over saldi, RocksDB-understøttet. Dokumentationen i koden siger: *"Currently DORMANT — no
> production code path calls it."*
>
> I SIGIL fandt vi i dag `spend_full_v5.rs` — 1160 linjer, tests grønne, committet. Aldrig
> kaldt fra produktionsstien.

**Nøjagtig samme fejlklasse. To forskellige kæder. Fundet uafhængigt af hinanden, med en uges
mellemrum.** "Færdigt, testet, committet — og uden for rækkevidde."

Det er hele argumentet. Det, der overføres mellem de to projekter, er ikke kode. Det er
*fejlklasser* og metoder til at finde dem. Og de er billigere at finde i SIGIL:

| | SIGIL | Quillon Graph |
|---|---|---|
| Rolle | laboratorium | produktion |
| Alder | nulstillet for to dage siden | kører siden genesis |
| Værdi i fare ved en fejl | testnet | rigtige brugeres penge |
| Kan nulstilles | ja, uden premine | nej, aldrig |
| Tilstand udledt af kæden | det er dét, vi bygger | det er dét, der mangler |

SIGIL er ikke et sideprojekt. Det er dér, jeg må lave de fejl, jeg ikke må lave i Quillon. En
konsensusændring i SIGIL koster en genstart. Den samme fejl i Quillon koster nogens penge.

**Og Flux er grunden til, at det overhovedet er praktisk muligt.** Det er byggeværktøjet, jeg
har skrevet, og det gør forskellen mellem at afprøve én idé om dagen og at afprøve flere i
timen. Uden det ville to kæder være ren overmod. Med det er den langsomme del ikke længere at
bygge — det er at beslutte, hvad der er rigtigt.

**Målet er ikke to kæder.** Målet er, at SIGIL besvarer spørgsmålet *"hvordan gør man al
tilstand udledbar fra kæden og verificerbar mellem noder"* under forhold, hvor svaret må være
forkert et par gange — og at Quillon derefter arver svaret. Det er den vej, jeg vil have en
AI-analyse til at gå: kortlægge, hvad SIGIL gør rigtigt her, og oversætte det til Quillons
arkitekturhul.

## Hvad der faktisk står færdigt i dag

**Færdigt og verificeret:**

- Minedrift udbetales gennemsigtigt fra en fastsat blokhøjde — støvkilden er lukket. Gamle
  blokke validerer uændret.
- Tegnebogen ved nu, hvilken note den skal bruge, og kan mønte en stor nok, hvis ingen passer.
- Privatlivsrettelsen er endelig koblet på produktionsstien, med en test der ikke kan omgås.
- To-input-kredsløbet beviser og verificerer: 50 + 47 ind, **én note på 94 ud** — en
  transaktion, det gamle kredsløb slet ikke kan udtrykke.
- Fjern-nedbrudshullet er lukket.

**Ikke færdigt, og jeg vil hellere sige det end lade være:**

- Kæden kan endnu ikke *bruge* to-input-kredsløbet. Transaktionsformatet bærer kun én
  nullifier, og tilstandslaget skriver dem ikke atomart endnu. Kredsløbet er færdigt. Vejen
  ind i kæden er det ikke.
- De 836.536 eksisterende støvnoter er ikke kryptografisk strandede — enhver enkelt ejer kan
  hente sit eget tilbage med gentagne sammenlægninger. Men puljen kan ikke ryddes samlet:
  én sammenlægning fjerner én note. Forhindringen er økonomisk, ikke matematisk. Det er en
  beslutning, jeg skal tage: afskrive dem, eller migrere dem tilbage.
- SIGIL er testnet. Der er én rigtig blokproducent. Det er ikke decentralt endnu, og jeg vil
  ikke skrive noget, der antyder andet.

## Hvad jeg gerne vil have fra dig

Du kan noget, jeg ikke kan: du kan se på det som en, der underviser i, hvornår et argument
holder.

To konkrete ting.

**Det matematiske:** to-input-kredsløbet lægger to beløb sammen og tjekker bevarelse *i et
endeligt legeme*. For at lighed i legemet skal betyde lighed i heltal, må ingen af siderne
kunne løbe rundt. Jeg havde først grænsen sat efter det samlede antal led — gennemgangen
påpegede, at den bindende side er den med *flest* led, altså udgange plus gebyr. Jeg tror, det
er rigtigt, og jeg vil gerne have et par øjne mere på det.

**Det pædagogiske:** hvis 1-ind/2-ud-begrænsningen tog mig så lang tid at se, selvom den er ren
aritmetik — hvordan forklarer man den så, så en, der kender Bitcoin, forstår den på tredive
sekunder? Jeg har prøvet med UTXO-analogien ovenfor. Jeg ved ikke, om den er skarp nok.

---

*Kildehenvisninger, hvis du vil grave: alle tal er hentet fra den kørende node den 28. august
2026 via `/v1/shielded/anchor` og `/v1/supply`; blokraten er målt over 90 sekunder ved højde
317.141. Kredsløb, tests og de fulde begrundelser ligger i commit-historikken på grenen
`hardening/ws-2026-07-18` — commit-beskederne dér er skrevet til at kunne læses som
laboratoriejournal, inklusive det, der gik galt.*
