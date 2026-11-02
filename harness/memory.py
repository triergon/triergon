"""Operation-scoped Darwin/Linux memory measurements with conservative RSS bounds.

The native observer is external tooling. Heap and RSS are separate instrumented
runs; timing workers never load it. Invalid/unknown coverage remains a non-pass.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
ALLOCATORS = {"malloc", "calloc", "realloc", "free", "aligned_alloc", "posix_memalign"}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def allocator_symbols(listing, system):
    """Keep raw versioned symbols; identify entry points without losing coverage."""
    if system == "Linux":
        raw = []
        for line in listing.splitlines():
            fields = line.split()
            if not fields:
                continue
            if len(fields) < 2 or fields[1] not in {"U", "w", "v"}:
                raise ValueError("unparsed GNU nm undefined-symbol record: " + line)
            raw.append(fields[0])
        names = {name.split("@", 1)[0] for name in raw}
        allocation = {name for name in names if name != "malloc_usable_size" and re.search(
            r"alloc|free$|strn?dup|asprintf|^(?:__)?(?:mmap(?:64)?|munmap|mremap|brk|sbrk)$|^_Z[nNdD]", name)}
        allocation.update(names & set(GUARD_ORDER))
        allocation.update(names & {"clone", "clone3", "__clone", "thrd_create", "fork", "vfork", "execve", "execveat"})
    elif system == "Darwin":
        raw = re.findall(r"^\s*(_\w+)\s*$", listing, re.M)
        allocation = {name.lstrip("_") for name in raw if re.search(r"alloc|free$|strdup|strndup|asprintf", name)}
    else:
        raise ValueError("unsupported allocator symbol platform")
    return {"raw_undefined_symbols": raw, "allocator_imports": sorted(allocation),
            "uncovered": sorted(allocation - ALLOCATORS), "raw_listing": listing}


def linux_loader_environment():
    """Inherited interposers cannot silently participate in a new certificate."""
    if any(os.environ.get(key) for key in ("LD_PRELOAD", "LD_AUDIT")):
        raise ValueError("inherited LD_PRELOAD/LD_AUDIT changes instrumentation")
    preload = Path("/etc/ld.so.preload")
    if preload.exists() and any(line.split("#", 1)[0].strip() for line in preload.read_text().splitlines()):
        raise ValueError("system-wide ld.so.preload is not covered")
    return {key: value for key, value in os.environ.items()
            if key.startswith(("LD_", "MALLOC_")) or key == "GLIBC_TUNABLES"}


def linux_main_elf_closure(worker, libraries, program_headers):
    """Validate the inspected ELF and every DT_NEEDED file, not an empty name.

    GNU ld.so can print the inspected main object's empty link-map name. This
    classification is limited to a zero-based ELF64 DYN object with a complete
    resolved dependency closure; raw readelf/loader output remains preserved.
    """
    if len(libraries) > 128:
        raise ValueError("ELF dependency closure exceeds 128 files")
    loads = re.findall(r"^\s*LOAD\s+(0x[0-9a-fA-F]+)\s+(0x[0-9a-fA-F]+)", program_headers, re.M)
    if not loads or any(int(value, 16) != 0 for value in loads[0]):
        raise ValueError("anonymous main mapping requires first ELF LOAD offset/vaddr zero")
    root = str(Path(worker).resolve())
    raw = subprocess.check_output(["readelf", "--file-header", "--dynamic", "--wide", str(worker)], text=True)
    if not re.search(r"^\s*Class:\s+ELF64\s*$", raw, re.M) or not re.search(r"^\s*Type:\s+DYN\b", raw, re.M):
        raise ValueError("anonymous main mapping requires an inspected ELF64 DYN object")
    paths = {row["realpath"] for row in libraries}
    dynamic = {root: raw}
    for path in sorted(paths - {root}):
        dynamic[path] = subprocess.check_output(["readelf", "--dynamic", "--wide", path], text=True)
    facts = {}
    for path, text in dynamic.items():
        if "Dynamic section" not in text:
            raise ValueError("unreadable ELF dynamic section: " + path)
        needed = re.findall(r"\(NEEDED\)[^\n]*\[([^\]\n]+)\]", text)
        soname = re.findall(r"\(SONAME\)[^\n]*\[([^\]\n]+)\]", text)
        if len(needed) > 128 or len(soname) > 1 or text.count("(NEEDED)") != len(needed):
            raise ValueError("unparsed or excessive ELF DT_NEEDED records: " + path)
        facts[path] = {"needed": needed, "soname": soname[0] if soname else None}
    aliases = {}
    for row in libraries:
        path = row["realpath"]
        for name in (row["name"], row["path"], path, Path(row["path"]).name, facts[path]["soname"]):
            if name is None:
                continue
            if name in aliases and aliases[name] != path:
                raise ValueError("ambiguous ELF dependency name: " + name)
            aliases[name] = path
    visited, pending = set(), [root]
    while pending:
        path = pending.pop()
        if path in visited:
            continue
        visited.add(path)
        for name in facts[path]["needed"]:
            if name not in aliases:
                raise ValueError("unresolved ELF DT_NEEDED: " + name)
            pending.append(aliases[name])
    # The interpreter is an implicit dependency even when absent in DT_NEEDED.
    interpreters = re.findall(r"\[Requesting program interpreter: ([^\]\n]+)\]", program_headers)
    implicit = {str(Path(path).resolve()) for path in interpreters}
    # Explicit-loader inspection of a shared object has no PT_INTERP; its sole
    # resolved ld-linux file is still hashed and may be implicit.
    implicit.update(row["realpath"] for row in libraries if re.fullmatch(r"ld(?:-linux[^/]*)?\.so(?:\.\d+)*", Path(row["path"]).name))
    if paths - visited - implicit:
        raise ValueError("unexplained extra ELF dependency files: " + str(sorted(paths - visited - implicit)))
    return {"inspected_class": "ELF64", "inspected_type": "DYN", "first_LOAD_vaddr": 0,
            "needed_closure": facts, "raw_dynamic_sections": dynamic}


def linux_linked_libraries(worker, interpreter=None):
    """Ask the ELF interpreter to resolve dependencies, then hash every file.

    This is invoked only by the parent build/qualification queue. The loader's
    --list operation does not execute the selected worker's main function.
    """
    worker = Path(worker)
    root_sha = sha(worker)
    binding = linux_loader_environment()
    if worker.stat().st_mode & (stat.S_ISUID | stat.S_ISGID):
        raise ValueError("secure-execution worker cannot certify LD_PRELOAD")
    headers = subprocess.check_output(["readelf", "--program-headers", "--wide", str(worker)], text=True)
    interpreters = re.findall(r"\[Requesting program interpreter: ([^\]\n]+)\]", headers)
    if interpreter is None and len(interpreters) != 1:
        raise ValueError("missing or ambiguous ELF interpreter; preload coverage unproven")
    if interpreter is not None and (len(interpreters) > 1 or (interpreters and interpreters[0] != interpreter)):
        raise ValueError("ELF interpreter differs from calibration executable")
    loader = Path(interpreter if interpreter is not None else interpreters[0])
    if not loader.is_absolute() or not loader.is_file():
        raise ValueError("unavailable ELF interpreter")
    listing = subprocess.check_output([str(loader), "--list", str(worker)], text=True)
    libraries = []
    virtual = []
    anonymous = None
    lines = [line for line in listing.splitlines() if line.strip()]
    if len(lines) > 256:
        raise ValueError("ELF listing exceeds 256 records")
    for index, line in enumerate(lines):
        main = re.fullmatch(r"\s+\((0x[0-9a-fA-F]{1,16})\)\s*", line)
        if main:
            if index != 0 or anonymous is not None or int(main[1], 16) == 0:
                raise ValueError("unexpected or duplicate anonymous ELF mapping")
            anonymous = {"line_raw": line, "address_raw": main[1]}
            continue
        if not line.strip():
            continue
        match = re.fullmatch(r"\s*(?:(\S+)\s+=>\s+)?(\S+)\s+\(0x[0-9a-fA-F]+\)\s*", line)
        if not match:
            raise ValueError("unparsed or unresolved ELF dependency: " + line)
        name, value = match.groups()
        if value in {"linux-vdso.so.1", "linux-gate.so.1"} and name is None:
            virtual.append(value)
            continue
        path = Path(value)
        if not path.is_absolute() or not path.is_file():
            raise ValueError("unreadable ELF dependency: " + value)
        libraries.append({"name": name or path.name, "path": str(path),
                          "realpath": str(path.resolve()), "bytes": path.stat().st_size,
                          "sha256": sha(path)})
    if not libraries:
        raise ValueError("no resolved ELF dependency files")
    if not any(row["realpath"] == str(loader.resolve()) for row in libraries):
        libraries.append({"name": loader.name, "path": str(loader), "realpath": str(loader.resolve()),
                          "bytes": loader.stat().st_size, "sha256": sha(loader)})
    main_mapping = linux_main_elf_closure(worker, libraries, headers)
    if sha(worker) != root_sha:
        raise ValueError("inspected ELF changed during dependency discovery")
    return {"status": "recorded", "dynamic_links": listing, "program_headers": headers,
            "inspected_artifact": {"path": str(worker.resolve()), "sha256": root_sha},
            "main_elf_inspection": main_mapping,
            "anonymous_main_mapping": None if anonymous is None else {**anonymous, **main_mapping},
            "interpreter": str(loader), "interpreter_sha256": sha(loader),
            "libraries": libraries, "virtual_kernel_libraries": virtual,
            "kernel_release": platform.release(), "binding_environment": binding}


def linux_library_identity(data):
    """Bind file contents/environment, not incidental loader ASLR addresses.

    Raw loader observations are retained separately by the build/qualification
    caller; this is an executable-dependency identity, not a parity projection.
    """
    identity = {key: data[key] for key in ("status", "interpreter", "interpreter_sha256", "libraries", "inspected_artifact",
            "virtual_kernel_libraries", "kernel_release", "binding_environment") if key in data}
    inspection = data.get("main_elf_inspection") or data.get("anonymous_main_mapping")
    if inspection is not None:
        identity["inspected_main_ELF"] = {key: inspection[key]
            for key in ("inspected_class", "inspected_type", "first_LOAD_vaddr", "needed_closure")}
    return identity


def valid_linux_receipt(data, mode):
    numeric = ("heap_peak_bytes", "heap_live_start", "heap_live_end", "calls", "window_allocation_calls",
               "rss_start", "rss_peak_observed", "rss_end", "hwm_start", "hwm_end", "page_size",
               "virtual_peak_start", "virtual_peak_end", "rss_samples", "error_bits", "begins", "ends",
               "unknown_releases", "recursive_calls", "selected_thread_scope", "entry_capacity")
    return (isinstance(data, dict) and data.get("schema") == 2 and data.get("mode") == mode
            and data.get("status") == "measured" and data.get("observer") in {"linux-glibc-v2", "linux-glibc-v3"}
            and (data["observer"] != "linux-glibc-v3" or guard_component_valid(data))
            and mode in {"heap", "rss"}
            and data.get("rss_source") == "proc-self-smaps_rollup-Rss"
            and data.get("hwm_source") == "getrusage-RUSAGE_SELF-ru_maxrss-KiB-times-1024"
            and data.get("hwm_scope") == "process-lifetime-reported"
            and data.get("rss_exact_peak_status") == "unresolved"
            and data.get("hwm_upper_bound_status") == "unproven"
            and data.get("virtual_peak_source") == "proc-self-status-VmPeak"
            and data.get("rss_bound_scope") == "same-process-ordinary-mappings"
            and data.get("rss_bound_formula") == "candidate-ceiling-minus-start-plus-two-pages-v1"
            and data.get("error_bits") == 0 and data.get("begins") == 1 and data.get("ends") == 1
            # C invalidates active-window unknown releases via error_bits;
            # unknown_releases also counts harmless releases before the window.
            and data.get("recursive_calls") == 0
            and data.get("selected_thread_scope") == 1 and bool(data.get("glibc"))
            and data.get("symbol_order") == ["malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc"]
            and all(isinstance(data.get(key), list) and len(data[key]) == 6
                    and all(type(value) is int and value >= 0 for value in data[key])
                    for key in ("symbol_attempts", "symbol_successes", "symbol_failures"))
            and all(type(data.get(key)) is int and data[key] >= 0 for key in numeric)
            and data["page_size"] > 0 and data["page_size"] & (data["page_size"] - 1) == 0
            and data["hwm_end"] >= data["hwm_start"]
            and data["virtual_peak_end"] >= data["virtual_peak_start"]
            and data["rss_peak_observed"] >= max(data["rss_start"], data["rss_end"])
            and linux_resident_basis_valid(data, mode)
            and (mode == "heap" or (
                type(data.get("rss_peak_upper_bound_bytes")) is int
                and data["rss_peak_upper_bound_bytes"] >= data["rss_peak_observed"])))


def observe(worker, operation, path, mode, arguments=(), kernel_bound=False):
    system = platform.system()
    library = ROOT / "target/harness" / ("allocations.so" if system == "Linux" else "allocations.dylib")
    if system not in {"Darwin", "Linux"} or not library.exists():
        return {"status": "unsupported-platform"}
    if system == "Linux":
        try:
            linux_loader_environment()
            if re.search(r"[\s:]", str(library)):
                raise ValueError("LD_PRELOAD path contains an unescapable separator")
        except (OSError, ValueError) as error:
            return {"status": "observer-environment-invalid", "error": str(error)}
    with tempfile.TemporaryDirectory(prefix="triergon-memory-") as directory:
        receipt = Path(directory) / "window.json"
        env = dict(os.environ, SAFE_MEDIA_ALLOCATION_REPORT=str(receipt), SAFE_MEDIA_MEMORY_MODE=mode)
        if system == "Linux":
            # Guard calibration/default collection remains a separate profile.
            # Inherited opt-in never silently changes either heap or controls.
            env.pop("SAFE_MEDIA_RSS_KERNEL_BOUND", None)
            if kernel_bound:
                if mode != "rss":
                    return {"status": "observer-environment-invalid", "error": "kernel bound is RSS-only"}
                env["SAFE_MEDIA_RSS_KERNEL_BOUND"] = "1"
        env["LD_PRELOAD" if system == "Linux" else "DYLD_INSERT_LIBRARIES"] = str(library)
        try:
            argv = [str(worker), *map(str, arguments)] if operation is None else [str(worker), operation, str(path), *map(str,arguments)]
            result = subprocess.run(argv, capture_output=True, timeout=30, env=env)
            if result.returncode or not receipt.exists():
                if system == "Linux":
                    return {"status": "observer-failed", "exit_code": result.returncode,
                            "stdout": result.stdout.decode(errors="replace"), "stderr": result.stderr.decode(errors="replace"),
                            "receipt_raw": receipt.read_text(errors="replace") if receipt.exists() else None}
                return {"status": "observer-failed", "stderr": result.stderr.decode(errors="replace")[-2048:]}
            data = json.loads(receipt.read_text())
            output = json.loads(result.stdout.decode().splitlines()[-1])
            if not isinstance(output, dict):
                return {"status": "observer-invalid", "receipt": data, "work": output}
            if system == "Linux" and mode == "rss" and isinstance(data, dict) and data.get("status") == "unresolved-hwm":
                return {"status": "rss-attribution-unresolved", "receipt": data, "observer": data,
                        "work": output, "instrument_sha256": sha(library)}
            if (not isinstance(data, dict) or data.get("schema") != 2 or data.get("status") != "measured"
                    or data.get("mode") != mode or (system == "Linux" and (
                        not valid_linux_receipt(data, mode)
                        or bool(data.get("kernel_rss_requested", 0)) != kernel_bound))):
                return {"status": "observer-invalid", "receipt": data, **({"work": output, "instrument_sha256": sha(library)} if system == "Linux" else {})}
            return {"status": "measured", "observer": data, "work": output, "instrument_sha256": sha(library)}
        except (OSError, ValueError, IndexError, subprocess.TimeoutExpired) as error:
            return {"status": "observer-failed", "error": str(error)}


def qualification(candidate, reference):
    if platform.system() != "Linux":
        return _darwin_qualification(candidate, reference)
    probe = ROOT / "target/harness/memory-check"
    library = ROOT / "target/harness/allocations.so"
    errors, parsed_targets, linked, records = [], [], {}, []
    heap, rss = (observe(probe, None, None, mode) for mode in ("heap", "rss"))
    if heap["status"] != "measured" or heap["observer"]["heap_peak_bytes"] != heap["work"].get("expected_peak_bytes") or heap["observer"]["heap_live_end"] != heap["observer"]["heap_live_start"]:
        errors.append("known-allocation peak or release mismatch")
    if rss["status"] != "measured" or rss["observer"]["rss_peak_observed"] <= rss["observer"]["rss_start"]:
        errors.append("known-allocation resident growth not observed")
    if rss["status"] == "rss-attribution-unresolved":
        errors.append("Linux resident upper-bound attribution unresolved; VmPeak is not a measured RSS HWM")
    if heap["status"] == "measured" and not all(value > 0 for value in heap["observer"]["symbol_successes"]):
        errors.append("Linux calibration did not exercise every admitted allocator hook")
    symbols, details, defined = {}, {}, {}
    guard_certificate = None
    identities = {}
    binding_environment = {}
    exports = None
    try:
        binding_environment = linux_loader_environment()
        identities = dict(observer_sha256=sha(library), probe_sha256=sha(probe),
            worker_hashes={"candidate": sha(Path(candidate)), "reference": sha(Path(reference))})
        roots = {"candidate": Path(candidate), "reference": Path(reference), "probe": probe, "observer": library}
        for name in ("candidate", "reference", "probe"):
            linked[name] = linux_linked_libraries(roots[name])
        linked["observer"] = linux_linked_libraries(library, linked["probe"]["interpreter"])
        objects = {str(path.resolve()): path for path in roots.values()}
        for data in linked.values():
            objects.update({row["realpath"]: Path(row["realpath"]) for row in data["libraries"]})
        for path, object_path in objects.items():
            defined[path] = subprocess.check_output(["nm", "-D", "--defined-only", "--format=posix", str(object_path)], text=True)
        exports = defined[str(library.resolve())]
        export_names = set(symbol_versions(exports))
        missing = (ALLOCATORS | {"write", "writev"} | set(GUARD_ORDER)) - export_names
        if missing:
            errors.append("observer: missing exported hooks " + str(sorted(missing)))
        for role in ("candidate", "reference"):
            path = str(roots[role].resolve())
            targets = {path, *[row["realpath"] for row in linked[role]["libraries"]]}
            for target in targets:
                raw = subprocess.check_output(["nm", "-D", "--undefined-only", "--format=posix", target], text=True)
                parsed = allocator_symbols(raw, "Linux")
                details[role + ":" + target] = parsed
                owner = {"realpath": target, "sha256": sha(Path(target))}
                parsed_targets.append((role, target, owner, parsed))
                if target == path:
                    symbols[role] = parsed["allocator_imports"]
                    if not symbols[role]: errors.append(role + ": selected worker allocator coverage empty/unproven")
        probe_undefined = subprocess.check_output(["nm", "-D", "--undefined-only", "--format=posix", str(probe)], text=True)
        bindings = dict(observer_path=str(library.resolve()), probe_path=str(probe.resolve()),
            observer_exports=exports, probe_undefined=probe_undefined, defined_objects=defined)
        # Cache only within this collector process, keyed by all sealed objects,
        # export/version transcripts and calibrated binary bytes. Each real
        # operation is still checked for zero active counters independently.
        cache_key = digest(dict(identities=identities, closure=closure_facts(linked), bindings=bindings,
                                environment=binding_environment))
        guard_certificate = _GUARD_CALIBRATION_CACHE.get(cache_key)
        if guard_certificate is None and heap["status"] == rss["status"] == "measured" and heap["observer"].get("observer") == rss["observer"].get("observer") == "linux-glibc-v3":
            records = [dict(mode="heap", case="default", observed=heap), dict(mode="rss", case="default", observed=rss)]
            for mode in ("heap", "rss"):
                for case in ("scope-startup", *GUARD_CASES):
                    observed = observe(probe, None, None, mode, arguments=(case,))
                    records.append(dict(mode=mode, case=case, observed=observed))
            guard_certificate = certify(records, linked, identities["observer_sha256"], identities["probe_sha256"], bindings)
            _GUARD_CALIBRATION_CACHE[cache_key] = guard_certificate
        if not certificate_valid(guard_certificate, linked, identities["observer_sha256"], identities["probe_sha256"]):
            errors.append("Linux active-window guard certificate unproven")
        for role, target, owner, parsed in parsed_targets:
            reviewed = conditional_coverage(parsed, owner, guard_certificate, linked,
                identities["observer_sha256"], identities["probe_sha256"])
            details[role + ":" + target] = reviewed
            if reviewed["remaining_uncovered"]:
                errors.append(target + ": uncovered allocator imports " + str(reviewed["remaining_uncovered"]))
    except (OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
        errors.append("Linux guard/link coverage unavailable: " + str(error))
    kernel_requested = os.environ.get("SAFE_MEDIA_RSS_KERNEL_BOUND") == "1"
    kernel_certificate, kernel_records = None, []
    if os.environ.get("SAFE_MEDIA_RSS_KERNEL_BOUND") not in {None, "", "0", "1"}:
        errors.append("invalid optional kernel RSS request")
    if kernel_requested and not errors:
        try:
            profile = load_kernel_profile(identities["observer_sha256"], identities["probe_sha256"])
            key = digest(dict(profile=profile, identities=identities, guard=guard_certificate["certificate_sha256"]))
            kernel_certificate = _KERNEL_CALIBRATION_CACHE.get(key)
            if kernel_certificate is None:
                for case in ("default", "writev", "transient"):
                    observed = observe(probe, None, None, "rss", arguments=() if case == "default" else (case,), kernel_bound=True)
                    kernel_records.append(dict(case=case, observed=observed))
                kernel_certificate = certify_kernel(kernel_records, profile, identities["observer_sha256"], identities["probe_sha256"])
                _KERNEL_CALIBRATION_CACHE[key] = kernel_certificate
            if not kernel_certificate_valid(kernel_certificate, identities["observer_sha256"], identities["probe_sha256"]):
                raise ValueError("kernel RSS certificate unavailable or stale")
        except (OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
            errors.append("optional kernel RSS coverage unavailable: " + str(error))
    return dict(status="qualified-selected-workers" if not errors else "unqualified", errors=errors,
        probe_heap=heap, probe_rss=rss, allocator_imports=symbols, allocator_import_details=details,
        linked_libraries=linked, linked_library_hashes=linked_hashes(linked), observer_exports_raw=exports,
        binding_environment=binding_environment, platform="Linux", architecture=platform.machine(),
        runtime_guard_required=True, runtime_guard_certificate=guard_certificate,
        attempted_guard_calibration=records, kernel_rss_required=kernel_requested,
        kernel_rss_certificate=kernel_certificate, attempted_kernel_calibration=kernel_records, **identities)


def _darwin_qualification(candidate, reference):
    probe = ROOT / "target/harness/memory-check"
    system = platform.system()
    library = ROOT / "target/harness" / ("allocations.so" if system == "Linux" else "allocations.dylib")
    binding = {}
    if system == "Linux":
        try:
            binding = linux_loader_environment()
        except (OSError, ValueError) as error:
            return {"status": "unqualified", "errors": [str(error)]}
    heap, rss = (observe(probe, None, None, mode) for mode in ("heap", "rss"))
    errors = []
    if heap["status"] != "measured" or heap["observer"]["heap_peak_bytes"] != heap["work"].get("expected_peak_bytes") or heap["observer"]["heap_live_end"] != heap["observer"]["heap_live_start"]:
        errors.append("known-allocation peak or release mismatch")
    if rss["status"] != "measured" or rss["observer"]["rss_peak_observed"] <= rss["observer"]["rss_start"]:
        errors.append("known-allocation resident growth not observed")
    if system == "Linux":
        if heap["status"] == "measured" and not all(value > 0 for value in heap["observer"]["symbol_successes"]):
            errors.append("Linux calibration did not exercise every admitted allocator hook")
        if rss["status"] == "rss-attribution-unresolved":
            errors.append("Linux resident upper-bound attribution unresolved; VmPeak is not a measured RSS HWM")
    # This certifies the selected workers' allocation entry points, not every
    # possible system-library path or a universal malloc implementation.
    symbols = {}
    detail, linked = {}, {}
    for name, worker in (("candidate", candidate), ("reference", reference)):
        try:
            targets = {name: Path(worker)}
            if system == "Linux":
                linked[name] = linux_linked_libraries(worker)
                targets.update({row["realpath"]: Path(row["realpath"]) for row in linked[name]["libraries"]})
            for target, path in targets.items():
                command = ["nm", "-D", "--undefined-only", "--format=posix", str(path)] if system == "Linux" else ["nm", "-u", str(path)]
                listing = subprocess.check_output(command, text=True)
                parsed = allocator_symbols(listing, system)
                detail[target] = parsed
                if target == name:
                    symbols[name] = parsed["allocator_imports"]
                    if system == "Linux" and not symbols[name]:
                        errors.append(name + ": selected worker allocator coverage empty/unproven")
                if parsed["uncovered"]:
                    errors.append(target + ": uncovered allocator imports " + str(parsed["uncovered"]))
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append(name + ": allocation/link coverage unavailable: " + str(error))
    exports = None
    if system == "Linux":
        try:
            linked["probe"] = linux_linked_libraries(probe)
            linked["observer"] = linux_linked_libraries(library, linked["probe"]["interpreter"])
            exports = subprocess.check_output(["nm", "-D", "--defined-only", "--format=posix", str(library)], text=True)
            names = {line.split()[0].split("@", 1)[0] for line in exports.splitlines() if line.split()}
            missing = (ALLOCATORS | {"write", "writev"}) - names
            if missing:
                errors.append("observer: missing exported hooks " + str(sorted(missing)))
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append("Linux instrumentation link/export coverage unavailable: " + str(error))
    def recorded_sha(path):
        try:
            return sha(Path(path))
        except OSError as error:
            errors.append(str(path) + ": identity unavailable: " + str(error))
            return None
    identities = {"probe_sha256": recorded_sha(probe), "observer_sha256": recorded_sha(library),
                  "worker_hashes": {"candidate": recorded_sha(candidate), "reference": recorded_sha(reference)}}
    return {"status": "qualified-selected-workers" if not errors else "unqualified", "errors": errors,
            "probe_heap": heap, "probe_rss": rss, "allocator_imports": symbols,
            **identities, **({"platform": "Linux", "architecture": platform.machine(),
                "linked_libraries": linked, "linked_library_hashes": linked_hashes(linked),
                "allocator_import_details": detail, "observer_exports_raw": exports,
                "binding_environment": binding} if system == "Linux" else {})}


def linked_hashes(linked):
    return {name: {row["realpath"]: row["sha256"] for row in data["libraries"]}
            for name, data in linked.items()}


def linux_certificate_current(certificate):
    linked = certificate.get("linked_libraries", {})
    if not {"candidate", "reference", "probe", "observer"} <= linked.keys():
        return False
    try:
        if certificate.get("kernel_rss_required") and not kernel_certificate_valid(
                certificate.get("kernel_rss_certificate"), certificate.get("observer_sha256"), certificate.get("probe_sha256")):
            return False
        if certificate.get("status") == "qualified-selected-workers" or certificate.get("runtime_guard_required"):
            if (certificate.get("runtime_guard_required") is not True
                or not certificate_valid(certificate.get("runtime_guard_certificate"), linked,
                                         certificate.get("observer_sha256"), certificate.get("probe_sha256"))
                or any(not data.get("inspected_artifact")
                       or sha(Path(data["inspected_artifact"]["path"])) != data["inspected_artifact"]["sha256"]
                       for data in linked.values())):
                return False
        return (certificate.get("binding_environment") == linux_loader_environment()
                and certificate.get("linked_library_hashes") == linked_hashes(linked)
                and all(str(Path(row["path"]).resolve()) == row["realpath"]
                        and sha(Path(row["realpath"])) == row["sha256"]
                        for data in linked.values() for row in data["libraries"]))
    except (OSError, ValueError, KeyError, TypeError):
        return False


def measure(candidate, reference, path, pairs, seed, certificate, interval, operation=None):
    if certificate["status"] != "qualified-selected-workers":
        return {"status": "observer-unqualified", "certificate": certificate}
    if certificate.get("platform") == "Linux" and not linux_certificate_current(certificate):
        return {"status": "instrumentation-changed", "certificate": certificate}
    prefix,arguments = operation or ("", [])
    input_sha256 = sha(path)
    rng = random.Random(seed)
    samples = []
    for _ in range(pairs):
        row = {}
        jobs = [(name, mode) for name in ("candidate", "reference") for mode in ("heap", "rss")]
        rng.shuffle(jobs)
        for name, mode in jobs:
            worker = candidate if name == "candidate" else reference
            if certificate.get("kernel_rss_required") and mode == "rss":
                row[name + "_" + mode] = observe(worker, prefix+"memory", path, mode, arguments, kernel_bound=True)
            else:
                row[name + "_" + mode] = observe(worker, prefix+"memory", path, mode, arguments)
        samples.append(row)
    result = evaluate(samples, pairs, seed, certificate, interval)
    result["input_sha256"] = input_sha256
    if sha(path) != input_sha256:
        result["status"] = "input-changed"
    if certificate.get("platform") == "Linux" and not linux_certificate_current(certificate):
        result["status"] = "instrumentation-changed"
    return result


def evaluate(samples, pairs, seed, certificate, interval):
    rss_bounds = []
    heaps = []
    totals = []
    guarded = certificate.get("platform") == "Linux" and certificate.get("status") == "qualified-selected-workers"
    if guarded and not certificate_valid(certificate.get("runtime_guard_certificate"), certificate.get("linked_libraries", {}),
                                        certificate.get("observer_sha256"), certificate.get("probe_sha256")):
        return {"status": "observer-unqualified", "samples": samples, "certificate": certificate}
    for row in samples:
        if certificate.get("platform") == "Linux":
            kernel_rows = [value.get("observer", {}) for key, value in row.items() if key.endswith("_rss")]
            if certificate.get("kernel_rss_required") or any(data.get("kernel_rss_requested") == 1 for data in kernel_rows):
                if (certificate.get("kernel_rss_required") is not True
                    or not kernel_certificate_valid(certificate.get("kernel_rss_certificate"), certificate.get("observer_sha256"), certificate.get("probe_sha256"))
                    or any(data.get("kernel_rss_requested") != 1 for data in kernel_rows)):
                    return {"status": "observer-unqualified", "samples": samples, "certificate": certificate}
        if guarded and any(not sample_valid(value.get("observer"), certificate.get("runtime_guard_certificate"),
                certificate.get("linked_libraries", {}), certificate.get("observer_sha256"), certificate.get("probe_sha256"))
                for value in row.values()):
            return {"status": "invalid-memory-observation", "samples": samples, "certificate": certificate}
        if certificate.get("platform") == "Linux" and (set(row) != {"candidate_heap", "candidate_rss", "reference_heap", "reference_rss"}
                or any(not valid_linux_receipt(value.get("observer"), key.rsplit("_", 1)[1])
                       or value.get("instrument_sha256") != certificate.get("observer_sha256") for key, value in row.items())):
            return {"status": "invalid-memory-observation", "samples": samples, "certificate": certificate}
        if any(v["status"] != "measured" for v in row.values()):
            return {"status": "measurement-failed", "samples": samples, "certificate": certificate}
        expected = row["reference_heap"]["work"]
        if any(v["work"] != expected for v in row.values()):
            return {"status": "unequal-memory-work", "samples": samples, "certificate": certificate}
        a, b = (row[k + "_rss"]["observer"] for k in ("candidate", "reference"))
        # Darwin retains its HWM bound. Linux uses only the explicitly reviewed
        # resident upper bound; reported HWM and virtual diagnostics stay raw.
        # The denominator is always observed resident growth, never VmPeak.
        candidate_peak = a["rss_peak_upper_bound_bytes"] if certificate.get("platform") == "Linux" else max(a["hwm_end"], a["rss_peak_observed"])
        upper = candidate_peak - a["rss_start"] + 2 * a["page_size"]
        lower = b["rss_peak_observed"] - b["rss_start"] - 2 * b["page_size"]
        if lower <= 0 or upper <= 0:
            return {"status": "rss-attribution-unresolved", "samples": samples, "certificate": certificate}
        rss_bounds.append((upper, lower))
        heaps.append(tuple(row[k + "_heap"]["observer"]["heap_peak_bytes"] for k in ("candidate", "reference")))
        totals.append((candidate_peak, b["rss_peak_observed"] if certificate.get("platform") == "Linux" else max(b["hwm_end"], b["rss_peak_observed"])))
    try:
        rss, heap, total = (interval(values, seed) for values in (rss_bounds, heaps, totals))
    except ValueError:
        return {"status": "invalid-memory-observation", "samples": samples, "certificate": certificate}
    return {"status": "qualified", "rss_ratio_upper_95": rss["upper_95"], "heap_ratio_upper_95": heap["upper_95"],
            "rss_bound_ratio": rss, "heap_ratio": heap, "total_rss_ratio": total, "pairs": pairs,
            "samples": samples, "certificate": certificate, "seed": seed,
            **({"rss_bound_basis": "candidate declared resident ceiling / reference observed smaps resident lower bound; +2/-2 pages",
                "total_rss_basis": "candidate resident ceiling / reference observed resident lower bound; not a VM ratio",
                "rss_exact_peak_status": "unresolved"} if certificate.get("platform") == "Linux" else {})}


def reusable_memory(measurement, pairs, instrumentation):
    """A rebuilt calibration or observer invalidates memory reuse, not timing."""
    if not instrumentation or measurement.get("status") != "qualified" or measurement.get("pairs") != pairs:
        return False
    certificate = measurement.get("certificate", {})
    same = (certificate.get("probe_sha256") == instrumentation.get("probe_sha256")
            and certificate.get("observer_sha256") == instrumentation.get("observer_sha256"))
    if instrumentation.get("platform") == "Linux":
        if certificate.get("kernel_rss_required") or instrumentation.get("rss_kernel_bound"):
            kernel = certificate.get("kernel_rss_certificate")
            if (not kernel_certificate_valid(kernel, certificate.get("observer_sha256"), certificate.get("probe_sha256"))
                or instrumentation.get("rss_kernel_bound") != {key: kernel["profile"][key] for key in ("proof_path", "proof_sha256")}):
                return False
        if certificate.get("status") == "qualified-selected-workers" and not certificate_valid(
                certificate.get("runtime_guard_certificate"), certificate.get("linked_libraries", {}),
                certificate.get("observer_sha256"), certificate.get("probe_sha256")):
            return False
        same &= certificate.get("platform") == "Linux" and bool(instrumentation.get("linked_library_hashes"))
        same &= all(certificate.get(key) == instrumentation.get(key) for key in ("architecture", "binding_environment"))
        same &= all(certificate.get("linked_library_hashes", {}).get(key) == value
                    for key, value in instrumentation.get("linked_library_hashes", {}).items())
    return same


# Native active-window coverage: certificate evidence stays separate from the raw import audit.
import json
from pathlib import Path

GUARD_ORDER = ("mmap", "mmap64", "munmap", "pthread_create", "dlopen", "dlmopen", "dlclose")
GUARD_POLICY = "invalidate-any-active-attempt-v1"
SCOPE_ERROR_BIT = 1 << 12
TLS_IMPORTS = frozenset(("_dl_allocate_tls", "_dl_allocate_tls_init", "_dl_deallocate_tls"))
GUARD_CASES = {
    "guard-mmap": (0, True), "guard-mmap64": (1, True), "guard-munmap": (2, True),
    "guard-mmap-failure": (0, False), "guard-mmap64-failure": (1, False), "guard-munmap-failure": (2, False),
    "guard-pthread-create": (3, True), "guard-dlopen": (4, True), "guard-dlmopen": (5, True),
    "guard-dlclose": (6, True), "guard-dlopen-failure": (4, False), "guard-dlmopen-failure": (5, False),
}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()


def closure_facts(linked):
    files, aliases, libcs, loaders = {}, {}, set(), set()
    for data in linked.values():
        if not isinstance(data, dict) or data.get("status") != "recorded":
            raise ValueError("unrecorded library closure")
        for row in data["libraries"]:
            path, content = row["realpath"], row["sha256"]
            if (not Path(path).is_absolute() or not isinstance(content, str) or len(content) != 64
                or any(char not in "0123456789abcdef" for char in content)):
                raise ValueError("unsealed library file")
            if path in files and files[path] != content:
                raise ValueError("conflicting library content")
            files[path] = content
            for spelling in (row["path"], path):
                if spelling in aliases and aliases[spelling] != path:
                    raise ValueError("ambiguous library path")
                aliases[spelling] = path
            if row["name"] == "libc.so.6": libcs.add(path)
            if content == data.get("interpreter_sha256") and data.get("interpreter") in (row["path"], path):
                loaders.add(path)
        artifact = data.get("inspected_artifact")
        if artifact:
            if artifact["path"] in files and files[artifact["path"]] != artifact["sha256"]:
                raise ValueError("conflicting inspected artifact")
            files[artifact["path"]] = artifact["sha256"]
    if len(libcs) != 1 or len(loaders) != 1:
        raise ValueError("one sealed libc/ELF interpreter pair required")
    return dict(files=files, aliases=aliases, libc=next(iter(libcs)), loader=next(iter(loaders)))


def symbol_versions(listing):
    result = {}
    for line in listing.splitlines():
        if not line.strip(): continue
        fields = line.split()
        if len(fields) < 2: raise ValueError("unparsed GNU symbol record")
        name, _, version = fields[0].partition("@")
        result.setdefault(name, set()).add(version.lstrip("@"))
    return result


def binding_facts(bindings, facts):
    if set(bindings["defined_objects"]) != set(facts["files"]):
        raise ValueError("incomplete/extra export-audited object closure")
    defined = {path: symbol_versions(raw) for path, raw in bindings["defined_objects"].items()}
    observer = symbol_versions(bindings["observer_exports"])
    if bindings["observer_exports"] != bindings["defined_objects"][bindings["observer_path"]]:
        raise ValueError("observer export transcript mismatch")
    tested = symbol_versions(bindings["probe_undefined"])
    for name in GUARD_ORDER:
        if name not in observer or not tested.get(name): raise ValueError("missing exported or exercised guard: " + name)
        providers = [path for path, symbols in defined.items() if name in symbols]
        if any(path not in {facts["libc"], facts["loader"], bindings["observer_path"]} for path in providers):
            raise ValueError("unexpected public guard provider")
        foundation_versions = set().union(*(defined[path].get(name, set()) for path in (facts["libc"], facts["loader"])))
        if not tested[name] <= foundation_versions:
            raise ValueError("tested guard ABI version has no sealed foundation provider")
    for name in TLS_IMPORTS:
        providers = [path for path, symbols in defined.items() if name in symbols]
        if providers != [facts["loader"]] or "GLIBC_PRIVATE" not in defined[facts["loader"]][name]:
            raise ValueError("private TLS provider is not uniquely the sealed interpreter")
    return {"tested_guard_versions": {name: sorted(tested[name]) for name in GUARD_ORDER},
            "binding_evidence_sha256": digest(bindings)}


def guard_fields(data):
    if (not isinstance(data, dict) or data.get("schema") != 2 or data.get("observer") != "linux-glibc-v3"
        or data.get("guard_policy") != GUARD_POLICY or data.get("guard_scope") != "observed-interposable-entrypoints"
        or data.get("guard_order") != list(GUARD_ORDER) or data.get("guard_resolved_mask") != 127
        or data.get("private_tls_basis") != "no-active-thread-or-loader-entrypoints"
        or data.get("private_tls_direct_hooks") is not False
        or type(data.get("begins")) is not int or data["begins"] != 1
        or type(data.get("ends")) is not int or data["ends"] != 1
        or type(data.get("selected_thread_scope")) is not int or data["selected_thread_scope"] != 1):
        raise ValueError("missing/stale guard profile, scope or boundary")
    for key in ("guard_calls", "guard_window_calls", "guard_window_successes", "guard_window_failures"):
        values = data.get(key)
        if (not isinstance(values, list) or len(values) != 7
            or any(type(value) is not int or not 0 <= value < (1 << 64) for value in values)):
            raise ValueError("malformed guard counters: " + key)
    if any(total < active or succeeded + failed != active for total, active, succeeded, failed in zip(
        data["guard_calls"], data["guard_window_calls"], data["guard_window_successes"], data["guard_window_failures"])):
        raise ValueError("inconsistent guard counters")


def zero_active(data):
    guard_fields(data)
    if (data.get("status") != "measured" or type(data.get("error_bits")) is not int or data["error_bits"] != 0
        or type(data.get("unknown_window_releases")) is not int or data["unknown_window_releases"] != 0
        or any(data["guard_window_calls"])):
        raise ValueError("active or invalid guard window")


def certify(records, linked, observer_sha256, probe_sha256, bindings):
    facts = closure_facts(linked)
    if (facts["files"].get(bindings["observer_path"]) != observer_sha256
        or facts["files"].get(bindings["probe_path"]) != probe_sha256):
        raise ValueError("guard export/probe transcript is not bound to the calibrated files")
    bound = binding_facts(bindings, facts)
    expected = {(mode, name) for mode in ("heap", "rss") for name in ("default", "scope-startup", *GUARD_CASES)}
    keyed = {}
    for record in records:
        key = (record["mode"], record["case"])
        if key not in expected or key in keyed: raise ValueError("duplicate/unexpected guard calibration")
        observed = record["observed"]
        if observed.get("instrument_sha256") != observer_sha256: raise ValueError("calibration observer changed")
        data = observed.get("observer", observed.get("receipt")); guard_fields(data)
        if data.get("mode") != record["mode"]: raise ValueError("calibration mode mismatch")
        work = observed.get("work", {})
        case = record["case"]
        if work.get("calibration") != case: raise ValueError("wrong native probe control")
        if case in ("default", "scope-startup"):
            if observed.get("status") != "measured" or work.get("expect_guard_invalid") is not False:
                raise ValueError("positive guard calibration failed")
            zero_active(data)
            if (work.get("expected_release_equal") is not True
                or data.get("heap_live_end") != data.get("heap_live_start")
                or (record["mode"] == "heap" and (
                    type(work.get("expected_peak_bytes")) is not int
                    or data.get("heap_peak_bytes") != work["expected_peak_bytes"]))
                or (record["mode"] == "rss" and not (
                    type(data.get("rss_start")) is int and type(data.get("rss_peak_observed")) is int
                    and data["rss_peak_observed"] > data["rss_start"]))):
                raise ValueError("positive allocation/release/resident oracle failed")
            if case == "scope-startup" and not all(value > 0 for value in data["guard_calls"]):
                raise ValueError("startup forwarding did not exercise every guard")
        else:
            index, successful = GUARD_CASES[case]
            requested = [0]*7; requested[index] = 1
            succeeded = requested if successful else [0]*7
            failed = [0]*7 if successful else requested
            if (work.get("expect_guard_invalid") is not True
                or work.get("guard_oracle_scope") != "authored-top-level-request-minima"
                or work.get("required_guard_window_calls") != requested
                or work.get("required_guard_window_successes") != succeeded
                or work.get("required_guard_window_failures") != failed):
                raise ValueError("probe oracle differs from the pinned top-level control")
            if (observed.get("status") != "observer-invalid" or data.get("status") != "invalid"
                or type(data.get("error_bits")) is not int or not data["error_bits"] & SCOPE_ERROR_BIT
                or any(actual < minimum for key_name, minimums in (
                    ("guard_window_calls", requested), ("guard_window_successes", succeeded), ("guard_window_failures", failed))
                    for actual, minimum in zip(data[key_name], minimums))):
                raise ValueError("negative control did not prove sticky guard rejection")
        keyed[key] = record
    if set(keyed) != expected: raise ValueError("missing guard calibration controls")
    certificate = dict(schema="linux-runtime-guard-certificate-v3", status="qualified-active-guards",
        observer_sha256=observer_sha256, probe_sha256=probe_sha256, guard_policy=GUARD_POLICY,
        closure=facts, closure_sha256=digest(facts), binding_facts=bound,
        binding_evidence=bindings, native_calibration_records=records,
        TLS_proof="sealed libc/interpreter plus closed active thread/loader paths; private symbols are not interposed")
    certificate["certificate_sha256"] = digest(certificate)
    return certificate


def certificate_valid(certificate, linked, observer_sha256, probe_sha256):
    if not isinstance(certificate, dict): return False
    try:
        return (certificate.get("status") == "qualified-active-guards"
            and certify(certificate["native_calibration_records"], linked, observer_sha256, probe_sha256,
                        certificate["binding_evidence"]) == certificate)
    except (KeyError, TypeError, ValueError): return False


def conditional_coverage(parsed, owner, certificate, linked, observer_sha256, probe_sha256):
    uncovered = set(parsed["uncovered"])
    covered = {}
    if certificate_valid(certificate, linked, observer_sha256, probe_sha256):
        versions = symbol_versions(parsed["raw_listing"])
        for name in uncovered & set(GUARD_ORDER):
            if versions.get(name) and versions[name] <= set(certificate["binding_facts"]["tested_guard_versions"][name]):
                covered[name] = dict(method="tested-public-entry-window-guard", certificate=certificate["certificate_sha256"])
        facts = certificate["closure"]
        if owner.get("realpath") == facts["libc"] and owner.get("sha256") == facts["files"][facts["libc"]]:
            raw = set(parsed["raw_undefined_symbols"])
            for name in uncovered & TLS_IMPORTS:
                if name + "@GLIBC_PRIVATE" in raw and versions.get(name) == {"GLIBC_PRIVATE"}:
                    covered[name] = dict(method="sealed-libc-loader-startup-scope", certificate=certificate["certificate_sha256"])
    return {**parsed, "runtime_guard_coverage": covered, "remaining_uncovered": sorted(uncovered - covered.keys())}


def sample_valid(data, certificate, linked, observer_sha256, probe_sha256):
    if not certificate_valid(certificate, linked, observer_sha256, probe_sha256): return False
    try:
        zero_active(data)
        return True
    except (KeyError, TypeError, ValueError): return False


_GUARD_CALIBRATION_CACHE = {}


def guard_component_valid(data):
    try:
        zero_active(data)
        return True
    except (KeyError, TypeError, ValueError):
        return False


KERNEL_RSS_BASIS = "min-kernel-positive-deltas-lifetime-VmPeak"
KERNEL_RSS_ROLES = {"module_source", "module_api", "observer_source", "observer_api", "probe_source",
                    "module_binary", "observer_binary", "probe_binary"}
KERNEL_RUNTIME_FILES = {"/sys/kernel/notes", "/proc/sys/kernel/random/boot_id",
                        "/sys/module/safe_media_rss_bound/notes/.note.gnu.build-id"}
_KERNEL_CALIBRATION_CACHE = {}


def linux_resident_basis_valid(data, mode):
    """Parse a declared upper bound; this alone grants no kernel admission."""
    requested = data.get("kernel_rss_requested", 0)
    if type(requested) is not int or requested not in {0, 1}:
        return False
    if not requested:
        return (data.get("rss_peak_upper_bound_basis") != KERNEL_RSS_BASIS and
                (mode == "heap" or (data.get("rss_peak_upper_bound_basis") == "process-lifetime-VmPeak"
                 and data.get("rss_peak_upper_bound_bytes") == data.get("virtual_peak_end"))))
    if mode != "rss" or data.get("observer") != "linux-glibc-v3" or data.get("rss_peak_upper_bound_basis") != KERNEL_RSS_BASIS:
        return False
    numeric = ("kernel_rss_abi", "kernel_rss_initial_bytes", "kernel_rss_positive_pages", "kernel_rss_upper_bytes",
               "kernel_rss_events", "kernel_rss_missed", "kernel_rss_errors", "kernel_rss_page_size")
    if any(type(data.get(key)) is not int or not 0 <= data[key] < 1 << 64 for key in numeric):
        return False
    page = data["kernel_rss_page_size"]
    upper = data["kernel_rss_initial_bytes"] + data["kernel_rss_positive_pages"] * page
    return (data["kernel_rss_abi"] == 1 and data["kernel_rss_missed"] == data["kernel_rss_errors"] == 0
            and page == data.get("page_size") and page > 0
            and data["kernel_rss_initial_bytes"] % page == 0 and upper < 1 << 64
            and upper == data["kernel_rss_upper_bytes"]
            and (data["kernel_rss_events"] > 0 or data["kernel_rss_positive_pages"] == 0)
            and isinstance(data.get("guard_calls"), list) and len(data["guard_calls"]) == 7
            # Conservatively close thread creation for the entire observer
            # lifetime, including constructor-to-BEGIN, not merely the window.
            and type(data["guard_calls"][3]) is int and data["guard_calls"][3] == 0
            and data.get("rss_peak_upper_bound_bytes") == min(upper, data.get("virtual_peak_end", -1)))


def elf_build_id_note(binary):
    """Read only a bounded ELF64 little-endian AArch64 module note section."""
    import struct
    if len(binary) < 64 or len(binary) > 16 * 1024 * 1024 or binary[:6] != b"\x7fELF\x02\x01":
        raise ValueError("module is not a bounded ELF64 little-endian file")
    if struct.unpack_from("<HH", binary, 16) != (1, 183):
        raise ValueError("module is not an AArch64 relocatable ELF")
    offset = struct.unpack_from("<Q", binary, 40)[0]
    width, count, names_index = struct.unpack_from("<HHH", binary, 58)
    if width != 64 or not 0 < count <= 4096 or names_index >= count or offset + count * width > len(binary):
        raise ValueError("invalid/extended module section table")
    def section(index):
        at = offset + index * width
        name = struct.unpack_from("<I", binary, at)[0]
        section_type = struct.unpack_from("<I", binary, at + 4)[0]
        start, size = struct.unpack_from("<QQ", binary, at + 24)
        if section_type == 8:  # SHT_NOBITS has no bytes in the object file.
            return name, b""
        if start + size > len(binary):
            raise ValueError("module section outside file")
        return name, binary[start:start+size]
    _, names = section(names_index)
    if len(names) > 1024 * 1024:
        raise ValueError("module section-name table too large")
    notes = []
    for index in range(count):
        name, data = section(index)
        if name >= len(names):
            raise ValueError("module section name outside table")
        end = names.find(b"\0", name)
        if end < 0:
            raise ValueError("unterminated module section name")
        if names[name:end] == b".note.gnu.build-id":
            notes.append(data)
    if len(notes) != 1 or len(notes[0]) < 16:
        raise ValueError("missing/ambiguous module GNU build-id note")
    name_size, descriptor_size, note_type = struct.unpack_from("<III", notes[0])
    if name_size != 4 or note_type != 3 or notes[0][12:16] != b"GNU\0" or not 1 <= descriptor_size <= 64 or len(notes[0]) != 16 + ((descriptor_size + 3) & ~3):
        raise ValueError("malformed module GNU build-id note")
    return notes[0]


def kernel_profile_current(profile, observer_sha256, probe_sha256):
    """Verify parent-produced source/coverage pins and the actually loaded note."""
    try:
        proof = profile["proof"]
        if (platform.system() != "Linux" or proof["schema"] != "linux-rss-kernel-bound-proof-v1"
            or proof["status"] != "reviewed-conditional-upper-bound" or type(proof["abi"]) is not int or proof["abi"] != 1
            or proof["kernel_release"] != platform.release() or proof["architecture"] != platform.machine()
            or type(proof["page_size"]) is not int or proof["page_size"] != os.sysconf("SC_PAGE_SIZE")
            or not KERNEL_RSS_ROLES <= proof["files"].keys() or not KERNEL_RUNTIME_FILES <= proof["runtime_files"].keys()):
            return False
        coverage = proof["coverage"]
        if (any(coverage.get(key) is not True for key in ("complete_hooks", "single_thread_since_constructor", "full_thp_disabled_before_open"))
            or coverage.get("lifecycle") != "constructor-open-to-END-ioctl"):
            return False
        pins = list(proof["files"].values()) + [coverage["review_artifact"]]
        for pin in pins:
            if sha(ROOT / pin["path"]) != pin["sha256"]:
                return False
        for path, expected in proof["runtime_files"].items():
            if path not in KERNEL_RUNTIME_FILES | {"/sys/module/safe_media_rss_bound/srcversion"} or sha(Path(path)) != expected:
                return False
        if (proof["files"]["observer_binary"]["sha256"] != observer_sha256
            or proof["files"]["probe_binary"]["sha256"] != probe_sha256
            or proof["files"]["module_api"]["sha256"] != proof["files"]["observer_api"]["sha256"]
            or sha(ROOT / profile["proof_path"]) != profile["proof_sha256"]
            or json.loads((ROOT / profile["proof_path"]).read_text()) != proof):
            return False
        live_note = Path("/sys/module/safe_media_rss_bound/notes/.note.gnu.build-id").read_bytes()
        return elf_build_id_note((ROOT / proof["files"]["module_binary"]["path"]).read_bytes()) == live_note
    except (OSError, KeyError, ValueError, TypeError):
        return False


def load_kernel_profile(observer_sha256, probe_sha256):
    identity = json.loads((ROOT / "target/harness/identity.json").read_text())
    pin = identity["instrumentation"]["rss_kernel_bound"]
    profile = dict(proof_path=pin["proof_path"], proof_sha256=pin["proof_sha256"],
                   proof=json.loads((ROOT / pin["proof_path"]).read_text()))
    if not kernel_profile_current(profile, observer_sha256, probe_sha256):
        raise ValueError("kernel RSS module/source/ABI/lifecycle/live identity proof unavailable or stale")
    return profile


def certify_kernel(records, profile, observer_sha256, probe_sha256):
    if not kernel_profile_current(profile, observer_sha256, probe_sha256):
        raise ValueError("kernel RSS parent proof is not current")
    expected = {"default": 4 * 1024 * 1024, "writev": 4 * 1024 * 1024, "transient": 32 * 1024 * 1024}
    seen = set()
    for record in records:
        case, observed = record["case"], record["observed"]
        if case not in expected or case in seen or observed.get("status") != "measured" or observed.get("instrument_sha256") != observer_sha256:
            raise ValueError("missing/duplicate/failed kernel RSS calibration")
        data, work = observed["observer"], observed["work"]
        if (not valid_linux_receipt(data, "rss") or data.get("kernel_rss_requested") != 1
            or work.get("calibration") != case or work.get("temporary_requested_bytes") != expected[case]
            or work.get("expected_release_equal") is not True or work.get("expect_guard_invalid") is not False
            or data["rss_peak_observed"] <= data["rss_start"] or data["kernel_rss_events"] <= 0
            or data["kernel_rss_positive_pages"] * data["kernel_rss_page_size"] < expected[case]):
            raise ValueError("kernel RSS calibration did not prove observed positive growth and release")
        seen.add(case)
    if seen != set(expected):
        raise ValueError("incomplete kernel RSS positive controls")
    result = dict(schema="linux-rss-kernel-certificate-v1", profile=profile,
                  observer_sha256=observer_sha256, probe_sha256=probe_sha256, native_calibration_records=records)
    result["certificate_sha256"] = digest(result)
    return result


def kernel_certificate_valid(certificate, observer_sha256, probe_sha256):
    if not isinstance(certificate, dict):
        return False
    try:
        return certify_kernel(certificate["native_calibration_records"], certificate["profile"], observer_sha256, probe_sha256) == certificate
    except (OSError, KeyError, ValueError, TypeError):
        return False
