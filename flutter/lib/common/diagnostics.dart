import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter_hbb/models/platform_model.dart';

/// Keep startup errors until the native bridge is ready. Each window has its own
/// isolate and installs these handlers independently.
class Diagnostics {
  static final List<String> _pending = [];
  static bool _ready = false;
  static bool _sending = false;

  static void install() {
    final previousFlutter = FlutterError.onError;
    FlutterError.onError = (details) {
      _record('${details.exceptionAsString()}\n${details.stack ?? ''}');
      previousFlutter?.call(details);
    };
    final previousPlatform = PlatformDispatcher.instance.onError;
    PlatformDispatcher.instance.onError = (error, stack) {
      _record('$error\n$stack');
      return previousPlatform?.call(error, stack) ?? false;
    };
  }

  static void ready() {
    _ready = true;
    unawaited(_flush());
  }

  static void _record(String message) {
    if (_pending.length == 20) _pending.removeAt(0);
    _pending.add(
      message.length > 32768 ? message.substring(0, 32768) : message,
    );
    if (_ready) unawaited(_flush());
  }

  static Future<void> _flush() async {
    if (_sending) return;
    _sending = true;
    try {
      while (_pending.isNotEmpty) {
        final error = await bind.mainRecordFlutterError(
          message: _pending.removeAt(0),
        );
        if (error.isNotEmpty) {
          debugPrint('Diagnostic error recording failed: $error');
        }
      }
    } catch (error) {
      // Never feed logging failures back into the uncaught-error handler.
      debugPrint('Diagnostic error recording failed: $error');
    } finally {
      _sending = false;
    }
  }
}
