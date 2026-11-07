# triergon-ogg

Ogg (RFC 3533) pages and packets, and the FLAC, Opus and Vorbis mappings.

[`PageHeader`] reads and checks one page; [`Assembler`] rebuilds the packets of one
logical stream from its pages under a memory budget, dropping a packet whose pages were
lost. [`read_packets`] reads every packet of a whole file in memory, for tests and tools.
